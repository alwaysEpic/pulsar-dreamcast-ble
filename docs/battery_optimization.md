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

---

## Overnight Drain Investigation — 2026-02-21

### Symptom
Fully charged battery (4.2V) drained to 166mV overnight while supposedly in System Off.
500mAh / ~8 hours = ~60mA average draw. Way above the ~5-10µA System Off target.

### Most Likely Cause: No Advertising Timeout

The main loop Phase 1 (BLE wait) has **no timeout**. If the device wakes from System Off (button
press, noise on SENSE pin, or power glitch) and nobody connects via BLE, it advertises at ~1-2mA
**forever**. At 1.5mA, a 500mAh battery lasts ~14 days — but combined with any other wake/boot
cycles, this fits the overnight drain pattern.

**Fix:** Add an advertising timeout — if no BLE connection within 5 minutes, enter System Off.

### Other Possible Contributors

**Pins not disconnected in System Off:**
- P0.14 (battery divider enable) — if floating/LOW, enables the 1M+510K divider (~2.8µA)
- P0.13 (charge current set) — dropped immediately after init, may float
- P0.06, P0.26, P0.30 (LEDs) — set HIGH but not disconnected

**Fix:** Disconnect all unnecessary pins in `enter_system_off()`:
```
Pins to disconnect: 3, 5, 6, 13, 14, 17, 20, 21, 22, 23, 24, 26, 30
Keep: P0.25 (QSPI CS, driven HIGH), P0.28 (boost SHDN, driven LOW)
```

**Charge current pin (P0.13) behavior:** Created as `Output`, immediately dropped. Embassy may
reset the pin on drop, leaving it floating. BQ25101 ISET behavior with floating input is undefined.

### Action Items
1. **Add 5-minute advertising timeout** — enter System Off if no BLE connection
2. **Disconnect all unused GPIO pins** before System Off
3. **Measure System Off current** with multimeter to verify fix
4. **Measure active gaming current** to get real battery life numbers

---

## Battery Testing Plan — 2026-02-21

### Goal
Verify battery life is at least 3-4 hours of active gaming, System Off draws <20µA,
and the battery is protected from damage in all scenarios.

### Equipment Needed

| Tool | Purpose | Cost | Priority |
|------|---------|------|----------|
| Multimeter (any) | Measure current in all states | Already have | Essential |
| USB power meter | Verify charging current and USB-powered draw | ~$10-15 | Nice to have |
| INA219 breakout | Continuous current logging via I2C | ~$3-8 | Nice to have |
| Nordic PPK2 | µA-resolution power profiling, sees BLE events | ~$99 | Gold standard |

### Test Procedures

#### Test 1: System Off Current (CRITICAL — do this first)
**Purpose:** Verify the overnight drain is fixed.

1. Put multimeter in µA (microamp) mode, 200µA or 2000µA range
2. Disconnect battery positive from board
3. Connect multimeter in series: battery(+) → multimeter(+), multimeter(-) → board BAT(+)
4. Boot the device, wait for it to start advertising
5. Enter System Off via 7s hold (or wait for advertising timeout once implemented)
6. Read the multimeter — should be **<20µA** for the whole board
7. If >100µA, something is still drawing power — investigate pin by pin

**Expected breakdown:**
- nRF52840 System Off: ~1.9µA
- QSPI flash in DPD: ~3µA
- BQ25101 quiescent: ~1µA
- LDO quiescent: ~2µA
- Total: ~8µA

**If reading is milliamps:** The device is not actually in System Off. Check RTT for boot loops,
verify `sd_power_system_off()` is being called, check SENSE pin configuration.

#### Test 2: Advertising Current
**Purpose:** Know the idle draw when waiting for BLE connection.

1. Multimeter in mA mode, in series with battery
2. Boot device, do NOT connect BLE
3. Wait for advertising to stabilize (~5s past fast reconnect)
4. Read — should be **0.5-2mA** (slow reconnect at 500ms interval)

#### Test 3: Active Gaming Current
**Purpose:** Calculate real battery life.

1. Multimeter in mA mode (use 10A jack if >200mA)
2. Boot, connect BLE, connect controller
3. Move stick / press buttons
4. Read — expect **120-190mA** (boost converter efficiency means battery
   draws more than the 5V rail: P_bat = P_5V / efficiency)

**Battery life = 500mAh / measured_mA**

If reading is 180mA → ~2.8 hours. If 130mA → ~3.8 hours.

#### Test 4: Charging Verification
**Purpose:** Confirm battery charges correctly and safely.

1. Discharge battery to ~3.3V (just above cutoff)
2. Connect USB power brick (not laptop)
3. Verify `PWR: Charging` in RTT log
4. Monitor battery voltage over time — should rise steadily
5. When BQ25101 STAT goes HIGH, battery should read 4.1-4.2V
6. Disconnect USB, wait 30 minutes, read voltage — should stay >4.1V

#### Test 5: Low Battery Cutoff
**Purpose:** Verify graceful shutdown protects the battery.

1. Let the device run on battery until cutoff triggers
2. Verify RTT shows `PWR: Low battery (XXXXmV), entering System Off`
3. Verify the cutoff voltage is ~3200mV (our threshold)
4. After cutoff, measure battery resting voltage — should be above 3.0V
   (voltage rebounds after load is removed)

#### Test 6: Wake and Reconnect Cycle
**Purpose:** Verify the full sleep/wake lifecycle works.

1. Enter System Off (manual 7s hold)
2. Press sync button — should wake and boot
3. Verify it reconnects to previously bonded host
4. Let it auto-sleep from inactivity timeout (10 min)
5. Wake again — verify reconnect still works
6. Repeat 2-3 times to confirm reliability

### Timed Battery Life Test
**Purpose:** Real-world battery life measurement.

1. Fully charge battery (4.2V, charging complete)
2. Disconnect USB
3. Boot, connect BLE, connect controller
4. Start a stopwatch
5. Use controller normally (or leave stick slightly deflected to prevent inactivity sleep)
6. Note the time when low battery cutoff triggers
7. That's your actual battery life

---

## General Low-Power Embedded Best Practices

Guidelines applicable to any battery-powered embedded project, not just this one.

### 1. Account for Every Microamp in Sleep

In deep sleep, every component matters. Create a sleep current budget:
- List every IC on the board and its quiescent/shutdown current from datasheets
- List every GPIO pin and its state (output high/low, input with pull, disconnected)
- Calculate current through every resistor divider, pull-up, and pull-down
- Measure and compare — the delta reveals hidden drains

Common hidden drains:
- **Flash/EEPROM chips** not in deep power down (2-5mA!)
- **Voltage regulators** with high quiescent current (some LDOs draw 100µA+)
- **Pull-up/pull-down resistors** through powered external ICs
- **Floating GPIO pins** that oscillate and cause internal shoot-through current
- **LED indicators** left in undefined states
- **I2C/SPI bus pull-ups** to powered peripherals

### 2. Pin Management Before Sleep

Every GPIO pin should be in one of these states before deep sleep:
- **Disconnected** (input, no pull) — lowest power, use for unused pins
- **Output driven** — only for pins that must maintain state (e.g., chip select HIGH)
- **Input with pull + SENSE** — only for wake sources

Never leave pins floating — a floating CMOS input can oscillate between high and low,
causing both P-channel and N-channel transistors to conduct simultaneously (shoot-through).
This can waste 10-100µA per floating pin.

### 3. Peripheral Shutdown Checklist

Before entering deep sleep, ensure every peripheral is properly shut down:
- ADC/SAADC: disabled (some draw 100µA+ when enabled but idle)
- Timers: stopped
- UART/SPI/I2C: disabled, pins disconnected
- Radio: handled by SoftDevice on nRF, but verify on other platforms
- DMA: no active transfers
- External peripherals: in shutdown/DPD mode, not just idle

### 4. Advertising Strategy for BLE Devices

BLE advertising is the second-biggest power consumer after active operation:

| Interval | Avg Current | Discovery Time | Use Case |
|----------|-------------|---------------|----------|
| 20ms | ~3mA | Instant | Pairing mode (time-limited!) |
| 100ms | ~0.5mA | <1s | Fast reconnect window |
| 500ms | ~0.15mA | 1-3s | Normal reconnect |
| 1000ms | ~0.08mA | 2-5s | Background / low priority |
| 2500ms | ~0.04mA | 5-15s | Ultra low power beacon |

**Best practice:** Tiered advertising with timeouts:
1. Fast (20-100ms) for 5-10 seconds after disconnect
2. Slow (500-1000ms) for 1-5 minutes
3. System Off if nobody connects

**Never advertise indefinitely** — this is the most common BLE battery drain bug.

### 5. Measure Before and After Every Change

Never assume a change saves power — measure it. The most common mistakes:
- Enabling DCDC but the inductor is wrong → worse efficiency than LDO
- Reducing advertising interval but increasing TX power → net increase
- Disconnecting a pin but enabling its pull-up → 100µA+ through the pull

A multimeter in series with the battery is the minimum viable power measurement setup.
If your device has distinct states (advertising, connected, active), measure each separately.

### 6. Voltage Regulator Selection

For battery-powered devices, regulator quiescent current matters enormously:

| Type | Typical Iq | Best For |
|------|-----------|----------|
| LDO (basic) | 50-500µA | Active mode, simplicity |
| LDO (low-Iq) | 1-10µA | Sleep mode, always-on rails |
| Buck (switching) | 10-50µA | High current loads, efficiency |
| Buck (nano-power) | 300nA-5µA | Ultra low power, always-on |

If your regulator has 100µA quiescent current, that alone limits battery life to
500mAh / 0.1mA = 5000 hours (208 days). Sounds fine, but it's the *floor* — everything
else adds to it.

### 7. LiPo Safety Essentials

| Threshold | Voltage | Action |
|-----------|---------|--------|
| Full charge | 4.20V | Stop charging (charger IC handles this) |
| Low battery warning | 3.50V | Alert user, reduce features |
| Shutdown cutoff | 3.20V | Enter deep sleep immediately |
| Damage threshold | 2.50V | Permanent capacity loss begins |
| Fire risk (charge) | >4.30V | Hardware protection required |
| Cold charge risk | <0°C | Do not charge below freezing |

**Voltage under load vs resting:** A battery reading 3.2V under 120mA load has an internal
resistance drop of ~24mV (at 200mΩ typical), so its resting voltage is ~3.22V. Account for
this when setting cutoff thresholds.

**Recovery from deep discharge:** Cells above 2.8V generally recover fully. Between 2.5-2.8V,
capacity may be reduced 5-10%. Below 2.5V, copper dendrites can form internally — the cell
may work but has increased failure risk. Replace if possible.

### 8. Power Budget Template

For any battery project, fill out this table:

```
| State          | Current | Duty Cycle | Weighted    |
|----------------|---------|------------|-------------|
| Deep sleep     | ___µA   | ___%       | ___µA       |
| Advertising    | ___mA   | ___%       | ___mA       |
| Connected idle | ___mA   | ___%       | ___mA       |
| Active         | ___mA   | ___%       | ___mA       |
| TOTAL WEIGHTED |         |            | ___mA       |
| Battery life   |         |            | ___mAh/___mA = ___ hours |
```

Fill in measured values, estimate duty cycles for your use case, calculate weighted average.
This is more accurate than "it draws X mA" because real usage mixes states.
