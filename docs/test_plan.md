# Dreamcast BLE Adapter -- Test Plan

Comprehensive test plan for the Dreamcast-to-BLE controller adapter (XIAO nRF52840 breadboard prototype).

**Revision:** 1.0
**Date:** 2026-02-23
**Target hardware:** Seeed XIAO nRF52840 + Pololu boost converter + BQ25101 charger + LiPo cell

---

## Equipment List

| Item | Purpose |
|------|---------|
| XIAO nRF52840 adapter (prototype) | Device under test (DUT) |
| Dreamcast controller (OEM) | Input source |
| Dreamcast controller (third-party, if available) | Compatibility |
| LiPo cell (1000mAh) | Battery power |
| Multimeter (DC voltage, current) | Voltage/current measurements |
| USB power meter (e.g., ChargerLAB KM003C) | USB draw, charge current |
| Laptop with Bluetooth (macOS/Windows/Linux) | BLE host, RTT console |
| Android phone with Bluetooth | BLE host, compatibility |
| iOS device (iPhone/iPad) | BLE host, compatibility |
| iBlueControlMod BLE receiver | Target receiver, compatibility |
| nRF Connect app (iOS/Android) | BLE service inspection |
| Gamepad Tester website (gpadtester.com) | HID input validation |
| J-Link / SWD debugger | RTT logging, reflash |
| Stopwatch / timer app | Timeout verification |
| Second 2.4 GHz device (Wi-Fi router, microwave) | Interference testing |
| USB-C cable | Charging, USB power source |
| Bench power supply (optional) | Controlled voltage testing |

---

## 1. Functional Testing -- Controller Inputs

### FUNC-01: Button Press Detection (ABXY)

| Field | Value |
|-------|-------|
| **Steps** | 1. Power on DUT, connect via BLE to laptop. 2. Open gpadtester.com. 3. Press and release each face button (A, B, X, Y) individually. 4. Verify each registers as a distinct button on the tester. |
| **Expected** | Each button maps to a unique HID button index. Press shows 1, release shows 0. No ghost presses. |
| **Pass/Fail** | All 4 buttons register correctly with no crosstalk. |
| **Equipment** | DUT, Dreamcast controller, laptop, gpadtester.com |

### FUNC-02: Start Button

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT to host. 2. Press Start. 3. Verify it maps to the correct HID button. |
| **Expected** | Start registers as a single button press/release. |
| **Pass/Fail** | Button registers on press and clears on release. |
| **Equipment** | DUT, Dreamcast controller, laptop, gpadtester.com |

### FUNC-03: D-Pad (Hat Switch)

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT. 2. Press each D-pad direction individually (Up, Down, Left, Right). 3. Press diagonal combinations (Up+Left, Up+Right, Down+Left, Down+Right). 4. Release and verify neutral. |
| **Expected** | Hat switch reports values 1-8 for 8 directions. Neutral reports null (0). |
| **Pass/Fail** | All 8 directions correct, null on release. No stuck directions. |
| **Equipment** | DUT, Dreamcast controller, laptop, gpadtester.com |

### FUNC-04: Analog Stick -- Center Position

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT. 2. Leave stick untouched. 3. Read Left Stick X and Y values on tester. |
| **Expected** | Both axes report approximately center (32768 of 0-65535 range, or 0.0 on normalized tester). |
| **Pass/Fail** | Stick reads within 5% of center when idle. |
| **Equipment** | DUT, Dreamcast controller, laptop, gpadtester.com |

### FUNC-05: Analog Stick -- Full Range

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT. 2. Move stick to full extent in all 4 cardinal directions. 3. Move stick in a full circle. 4. Check min/max values reported. |
| **Expected** | Full deflection approaches 0 and 65535 on each axis. Circular motion traces a smooth circle on tester visualization. |
| **Pass/Fail** | Full range achieved on both axes. No dead zones > 10% at extremes. No axis inversion. |
| **Equipment** | DUT, Dreamcast controller, laptop, gpadtester.com |

### FUNC-06: Analog Stick -- No "Flower" Pattern

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT. 2. Move stick slowly in a full circle. 3. Observe the XY plot on the gamepad tester. |
| **Expected** | Trace shows a circle, not a diamond/flower shape (which would indicate signed/unsigned mismatch). |
| **Pass/Fail** | Circular input produces circular output. |
| **Equipment** | DUT, Dreamcast controller, laptop, gpadtester.com |

### FUNC-07: Left Analog Trigger

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT. 2. Slowly press left trigger from released to full. 3. Release slowly. |
| **Expected** | Value smoothly ramps from 0 to 1023 (10-bit). Released = 0. |
| **Pass/Fail** | Smooth ramp, no jumps > 5% between samples. 0 at rest, near-max at full pull. |
| **Equipment** | DUT, Dreamcast controller, laptop, gpadtester.com |

### FUNC-08: Right Analog Trigger

| Field | Value |
|-------|-------|
| **Steps** | Same as FUNC-07 but for right trigger. |
| **Expected** | Same as FUNC-07. |
| **Pass/Fail** | Same as FUNC-07. |
| **Equipment** | DUT, Dreamcast controller, laptop, gpadtester.com |

### FUNC-09: Simultaneous Input Combinations

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT. 2. Hold A + B + left trigger + D-pad Up simultaneously. 3. Hold all face buttons + both triggers + stick deflected. 4. Release all. |
| **Expected** | All held inputs report simultaneously. No dropped inputs. All clear on release. |
| **Pass/Fail** | All inputs register concurrently, all clear on full release. |
| **Equipment** | DUT, Dreamcast controller, laptop, gpadtester.com |

### FUNC-10: Rapid Button Mashing

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT. 2. Rapidly press and release A button for 10 seconds as fast as possible. 3. Observe tester for missed presses or stuck states. |
| **Expected** | Presses and releases alternate with no stuck state. At 60Hz Maple Bus polling (16ms), presses shorter than one poll cycle will be missed -- this is a known limitation matching the Dreamcast controller's native poll rate, not a bug. |
| **Pass/Fail** | No stuck buttons. No phantom presses after stopping. Missed rapid presses are acceptable. |
| **Known Limitation** | Very rapid presses (~<16ms) may be missed due to 60Hz Maple Bus poll rate. This matches original Dreamcast hardware behavior. |
| **Equipment** | DUT, Dreamcast controller, laptop, gpadtester.com |

### FUNC-11: Right Stick (Hardcoded Center)

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT. 2. Move left stick around. 3. Observe Right Stick X/Y on tester. |
| **Expected** | Right stick always reports 32768/32768 (center) since Dreamcast has only one analog stick. |
| **Pass/Fail** | Right stick values remain constant at center. |
| **Equipment** | DUT, Dreamcast controller, laptop, gpadtester.com |

---

## 2. BLE Testing

### BLE-01: Initial Pairing (No Bond)

| Field | Value |
|-------|-------|
| **Steps** | 1. Flash DUT with cleared flash (or hold sync 3s to clear bond). 2. Power on. 3. Observe LED enters sync mode (blink) automatically. 4. Open Bluetooth settings on host, scan for "Xbox Wireless Controller". 5. Pair. |
| **Expected** | DUT auto-enters sync mode when no bond exists. Host discovers device, pairing completes with JustWorks. LED goes solid on connect. |
| **Pass/Fail** | Pairing completes within 30 seconds. LED behavior matches spec. |
| **Equipment** | DUT, laptop or phone |

### BLE-02: Bond Persistence Across Power Cycles

| Field | Value |
|-------|-------|
| **Steps** | 1. Pair DUT with host (BLE-01). 2. Power off DUT (remove battery). 3. Wait 10 seconds. 4. Power on DUT. 5. Observe reconnection without re-pairing. |
| **Expected** | DUT reconnects to previously bonded host without requiring new pairing. LED shows reconnecting state then solid on connect. |
| **Pass/Fail** | Automatic reconnection within 10 seconds of power-on. |
| **Equipment** | DUT, laptop or phone |

### BLE-03: Sync Mode Entry (3-Second Hold)

| Field | Value |
|-------|-------|
| **Steps** | 1. Power on DUT with existing bond. 2. Hold sync button for 3 seconds. 3. Observe LED behavior (slow blink -> fast blink at 3s). 4. Release button. |
| **Expected** | After 3s hold, DUT clears bond, enters sync mode (discoverable), LED blinks. RTT log shows "SYNC: Entering pairing mode (60s)". |
| **Pass/Fail** | Bond cleared, new host can discover and pair. |
| **Equipment** | DUT, SWD debugger (RTT), host device |

### BLE-04: Sync Mode Timeout (60 Seconds)

| Field | Value |
|-------|-------|
| **Steps** | 1. Enter sync mode (BLE-03). 2. Do NOT pair any device. 3. Wait 60 seconds. 4. Observe behavior. |
| **Expected** | After 60s, if no bond exists, DUT enters System Off (XIAO). RTT shows "BLE: No bond after sync timeout, entering System Off". |
| **Pass/Fail** | DUT sleeps after 60s timeout. Current draw drops to < 10 uA. |
| **Equipment** | DUT, stopwatch, multimeter (optional), SWD debugger |

### BLE-05: Reconnection After Host Disconnect

| Field | Value |
|-------|-------|
| **Steps** | 1. Pair and connect DUT. 2. On host, turn off Bluetooth. 3. Wait 5 seconds. 4. Turn Bluetooth back on. 5. Observe reconnection. |
| **Expected** | DUT detects disconnect, enters reconnecting state (LED off), automatically reconnects when host reappears. |
| **Pass/Fail** | Reconnection completes within 15 seconds of host Bluetooth re-enable. |
| **Equipment** | DUT, laptop or phone |

### BLE-06: Reconnect Timeout -- System Off (XIAO)

| Field | Value |
|-------|-------|
| **Steps** | 1. Pair DUT with host. 2. Turn off host Bluetooth. 3. Start a stopwatch. 4. Monitor RTT for "BLE: Reconnect timeout, entering System Off". 5. Verify System Off entered within ~60s (SLEEP_TIMEOUT_MS). 6. Press sync button to wake — verify fresh boot and reconnect. |
| **Expected** | DUT enters System Off after ~60s reconnect timeout. RTT shows timeout message. Current draw drops to < 10 uA. |
| **Pass/Fail** | System Off entered between 50-70s after disconnect. Wake via sync button starts fresh boot. |
| **Regression Note** | Previously failed: advertising had no timeout, so `advertise()` blocked indefinitely and the sleep check never ran. Fixed by adding 10s advertising timeout to reconnect modes so the loop iterates and checks elapsed time. |
| **Equipment** | DUT, stopwatch, multimeter (optional), SWD debugger |

### BLE-07: Connection Parameter Negotiation

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT to host. 2. Monitor RTT output for connection parameter update. 3. Optionally use nRF Connect to verify connection parameters. |
| **Expected** | Connection interval negotiated to 7-9 units (8.75-11.25ms). Slave latency = 0. Supervision timeout = 400 (4000ms). RTT shows no "Conn param update failed" message. |
| **Pass/Fail** | Parameters accepted by host (no error logged). |
| **Equipment** | DUT, SWD debugger, nRF Connect app (optional) |

### BLE-08: GATT Service Discovery

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect to DUT using nRF Connect app. 2. Discover all services. 3. Verify presence of HID (0x1812), Device Info (0x180A), Battery (0x180F). 4. Read HID Report Map characteristic. |
| **Expected** | All three services present. Report Map contains the Xbox One S BLE HID descriptor. HID Info reads 0x11 0x01 0x00 0x03. PnP ID shows VID=0x045E, PID=0x02E0. |
| **Pass/Fail** | All services and characteristics discoverable and readable. |
| **Equipment** | DUT, nRF Connect app on phone |

### BLE-09: HID Report Notifications

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT via nRF Connect. 2. Enable notifications on HID Report characteristic (0x2A4D). 3. Press buttons on Dreamcast controller. 4. Observe notification data. |
| **Expected** | Notifications arrive at approximately 125Hz (8ms interval). Button presses change the appropriate bytes in the 16-byte report. |
| **Pass/Fail** | Notifications are received. Data changes correspond to physical input. |
| **Equipment** | DUT, Dreamcast controller, nRF Connect app |

### BLE-10: Device Name -- Xbox Mode (Default)

| Field | Value |
|-------|-------|
| **Steps** | 1. Ensure default name preference (or toggle to Xbox). 2. Scan from host. |
| **Expected** | Device advertises as "Xbox Wireless Controller". |
| **Pass/Fail** | Correct name visible in scan results. |
| **Equipment** | DUT, host device |

### BLE-11: Device Name -- Dreamcast Mode (Triple-Press)

| Field | Value |
|-------|-------|
| **Steps** | 1. Triple-press sync button within 2 seconds. 2. Observe LED confirmation (5 rapid blinks). 3. DUT resets. 4. Scan from host. |
| **Expected** | Device advertises as "Dreamcast Wireless Controller". |
| **Pass/Fail** | Name changes after reset. Preference persists across further power cycles. |
| **Equipment** | DUT, host device |

### BLE-12: Name Toggle Persistence

| Field | Value |
|-------|-------|
| **Steps** | 1. Toggle name to Dreamcast (BLE-11). 2. Power cycle DUT. 3. Check advertised name. 4. Toggle back to Xbox. 5. Power cycle. 6. Verify Xbox name. |
| **Expected** | Name preference survives power cycles in both directions. |
| **Pass/Fail** | Both name states persist. |
| **Equipment** | DUT, host device |

### BLE-13: Range Test -- Close (1 Meter)

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT to host. 2. Place DUT 1 meter from host. 3. Press buttons and move stick continuously for 60 seconds. |
| **Expected** | No disconnections. No noticeable input lag. All inputs register. |
| **Pass/Fail** | Zero disconnects, zero missed inputs over 60 seconds. |
| **Equipment** | DUT, Dreamcast controller, laptop, gpadtester.com, tape measure |

### BLE-14: Range Test -- Medium (5 Meters)

| Field | Value |
|-------|-------|
| **Steps** | Same as BLE-13 at 5 meters distance. |
| **Expected** | No disconnections. Minimal input lag (< 30ms perceived). |
| **Pass/Fail** | Zero disconnects over 60 seconds. |
| **Equipment** | DUT, Dreamcast controller, laptop, tape measure |

### BLE-15: Range Test -- Far (10 Meters, Line of Sight)

| Field | Value |
|-------|-------|
| **Steps** | Same as BLE-13 at 10 meters with clear line of sight. |
| **Expected** | Connection may degrade. Acceptable: occasional lag. Not acceptable: frequent disconnects. |
| **Pass/Fail** | Fewer than 3 disconnects per minute at 10m. Reconnects automatically if lost. |
| **Equipment** | DUT, Dreamcast controller, laptop, tape measure |

### BLE-16: Interference Resilience

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT at 3 meters. 2. Place active Wi-Fi router between DUT and host. 3. Start a large file transfer over Wi-Fi. 4. Test inputs for 60 seconds. |
| **Expected** | BLE uses adaptive frequency hopping; moderate Wi-Fi interference should not cause disconnection. Some latency increase acceptable. |
| **Pass/Fail** | No disconnection during 60-second test. Inputs still register. |
| **Equipment** | DUT, Dreamcast controller, laptop, Wi-Fi router |

---

## 3. Power Management

### PWR-01: System Off via Sync Button (7-Second Hold)

| Field | Value |
|-------|-------|
| **Steps** | 1. Power on DUT. 2. Hold sync button for 7+ seconds. 3. LED blinks during hold, goes solid near end. 4. Release button. |
| **Expected** | DUT enters System Off. All LEDs extinguished. Boost converter disabled. Current draw < 10 uA. |
| **Pass/Fail** | System Off entered on release. Current confirmed < 10 uA with multimeter. |
| **Equipment** | DUT, multimeter, stopwatch |

### PWR-02: Wake from System Off

| Field | Value |
|-------|-------|
| **Steps** | 1. Enter System Off (PWR-01). 2. Press sync button briefly. 3. Observe boot sequence. |
| **Expected** | DUT performs full reset. Green LED blinks 3x (startup). Enters appropriate BLE state (sync or reconnect depending on bond). |
| **Pass/Fail** | DUT wakes and boots correctly within 3 seconds of button press. |
| **Equipment** | DUT |

### PWR-03: Advertising Timeout -- No Bond

| Field | Value |
|-------|-------|
| **Steps** | 1. Clear bonds and power on. 2. DUT enters sync mode. 3. Do not pair. 4. Wait for sync timeout (60s). |
| **Expected** | After 60 seconds of unanswered advertising with no bond, DUT enters System Off. |
| **Pass/Fail** | System Off at correct timeout. |
| **Equipment** | DUT, stopwatch |

### PWR-04: Controller Detection Timeout

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT via BLE to host (no Dreamcast controller plugged in). 2. Wait 60 seconds. |
| **Expected** | DUT searches for controller with exponential backoff. After DETECT_TIMEOUT_MS (60s), enters System Off. RTT shows "MAPLE: Detect timeout". |
| **Pass/Fail** | System Off at correct timeout. |
| **Equipment** | DUT, host device, SWD debugger, stopwatch |

### PWR-05: Inactivity Timeout (10 Minutes)

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT with controller. 2. Verify inputs work. 3. Set controller down, do not touch. 4. Wait 10 minutes. |
| **Expected** | After INACTIVITY_TIMEOUT_MS (600,000ms = 10 min) with no input change, DUT enters System Off. |
| **Pass/Fail** | System Off at correct timeout. Any input during the window resets the timer. |
| **Equipment** | DUT, Dreamcast controller, host, stopwatch/timer |

### PWR-06: Inactivity Timer Reset on Input

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT with controller. 2. Wait 9 minutes idle. 3. Press a button. 4. Release. 5. Wait another 9 minutes idle. 6. Verify DUT is still awake (total 18 minutes). |
| **Expected** | Button press at 9 minutes resets the inactivity timer. DUT stays awake through minute 18. |
| **Pass/Fail** | DUT remains active after timer reset. |
| **Equipment** | DUT, Dreamcast controller, host, timer |

### PWR-07: Battery Voltage Reading Accuracy

| Field | Value |
|-------|-------|
| **Steps** | 1. Measure battery voltage with multimeter at LiPo terminals. 2. Read RTT log for "BAT: XXXXmV" output. 3. Compare. |
| **Expected** | SAADC reading within 100mV of multimeter reading (accounting for divider tolerance). |
| **Pass/Fail** | Within 100mV across the 3.0-4.2V range. |
| **Equipment** | DUT, multimeter, SWD debugger |

### PWR-08: Battery Percentage Reporting via BLE

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT. 2. Read Battery Service (0x180F) Battery Level characteristic via nRF Connect. 3. Compare with RTT log percentage. |
| **Expected** | BLE battery level matches RTT logged percentage (0-100%). |
| **Pass/Fail** | Values match. Updates occur every 60 seconds (BATTERY_READ_INTERVAL_MS). |
| **Equipment** | DUT, nRF Connect app, SWD debugger |

### PWR-09: Low Battery Cutoff

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect a bench power supply set to 3.2V in place of battery (or use a partially discharged LiPo). 2. Power on DUT. 3. Slowly reduce voltage below 3.2V (LOW_BATTERY_CUTOFF_MV). |
| **Expected** | When battery reads below 3200mV and not charging, DUT enters System Off. RTT shows "PWR: Low battery". |
| **Pass/Fail** | System Off triggered at or near 3.2V. |
| **Equipment** | DUT, bench power supply or discharged LiPo, SWD debugger |

### PWR-10: Low Battery Cutoff Inhibited While Charging

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect USB charger. 2. Use a low-voltage battery (near 3.2V). 3. Verify DUT does NOT enter System Off while charging. |
| **Expected** | Charging flag (BQ25101 STAT pin LOW) inhibits low-battery cutoff. Battery level BLE characteristic reports 0xFF (charging indicator). |
| **Pass/Fail** | DUT stays awake while charging regardless of voltage. |
| **Equipment** | DUT, USB charger, low-voltage LiPo |

### PWR-11: USB VBUS Detection -- Boost Bypass

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect USB cable. 2. Connect via BLE. 3. Check RTT output for "PWR: USB detected, boost off (passthrough)". 4. Verify controller still gets 5V power (test inputs). |
| **Expected** | Boost converter remains off. Controller powered via Schottky diode passthrough from USB 5V. All inputs work normally. |
| **Pass/Fail** | Boost stays off, controller functional on USB power. |
| **Equipment** | DUT, USB cable, Dreamcast controller, SWD debugger |

### PWR-12: USB Plug/Unplug During Operation

| Field | Value |
|-------|-------|
| **Steps** | 1. Run DUT on battery (boost on, connected, controller working). 2. Plug in USB cable. 3. Verify RTT: "PWR: USB connected, disabling boost (passthrough)". 4. Verify inputs still work. 5. Unplug USB. 6. Verify RTT: "PWR: USB removed, enabling boost". 7. Verify inputs still work. |
| **Expected** | Seamless transition between boost and USB passthrough in both directions. No dropped BLE connection. No missed inputs. |
| **Pass/Fail** | No connection drop or input interruption during transition. |
| **Equipment** | DUT, USB cable, LiPo, Dreamcast controller, SWD debugger |

### PWR-13: Charge Status Detection

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect USB with LiPo attached. 2. Monitor RTT for "CHG: Charging started". 3. Wait for full charge or disconnect USB. 4. Monitor for "CHG: Charging stopped". |
| **Expected** | BQ25101 STAT pin correctly detected. Transitions logged. |
| **Pass/Fail** | Charge state transitions detected and logged correctly. |
| **Equipment** | DUT, USB cable, LiPo, SWD debugger |

### PWR-14: Current Draw -- Active (Battery)

| Field | Value |
|-------|-------|
| **Steps** | 1. Insert USB power meter or ammeter in series with battery. 2. Connect DUT via BLE, controller active. 3. Measure steady-state current. |
| **Expected** | Total draw should be approximately: nRF52840 (~5mA) + boost converter overhead (~10mA) + Dreamcast controller (~110mA) = ~125mA total from battery. |
| **Pass/Fail** | Measured current within 80-160mA range. |
| **Equipment** | DUT, multimeter/ammeter, LiPo, Dreamcast controller |

### PWR-15: Current Draw -- System Off

| Field | Value |
|-------|-------|
| **Steps** | 1. Enter System Off (PWR-01). 2. Measure current with sensitive ammeter or multimeter on uA range. |
| **Expected** | < 10 uA total (nRF52840 System Off ~1.5uA + QSPI flash DPD ~3uA + boost off + leakage). |
| **Pass/Fail** | < 10 uA measured. If > 10 uA, check for leaking GPIOs or undisconnected peripherals. |
| **Equipment** | DUT, sensitive multimeter (uA range) |

### PWR-16: QSPI Flash Deep Power Down

| Field | Value |
|-------|-------|
| **Steps** | 1. Power on DUT. 2. Observe RTT: "QSPI: Flash in Deep Power Down". 3. (Optional) Compare active current with a build that skips the DPD command. |
| **Expected** | Flash enters DPD at startup, saving 2-5 mA. |
| **Pass/Fail** | RTT message confirms DPD. Optional current comparison shows savings. |
| **Equipment** | DUT, SWD debugger, multimeter (optional) |

---

## 4. Durability and Stress Testing

### STRESS-01: Long Gaming Session (2 Hours)

| Field | Value |
|-------|-------|
| **Steps** | 1. Fully charge LiPo. 2. Connect DUT on battery power. 3. Play a game or continuously exercise inputs for 2 hours. 4. Monitor for disconnects, lag spikes, or erratic behavior. |
| **Expected** | Continuous operation without crashes or disconnections. Battery level decreases proportionally. |
| **Pass/Fail** | No crashes, no stuck inputs, no unexpected sleeps. Battery level tracks downward appropriately. |
| **Equipment** | DUT, Dreamcast controller, host, fully charged LiPo |

### STRESS-02: Rapid BLE Connect/Disconnect Cycles

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT to host. 2. Disable Bluetooth on host. 3. Wait for DUT to detect disconnect. 4. Re-enable Bluetooth. 5. Wait for reconnection. 6. Repeat 20 times. |
| **Expected** | DUT handles all 20 cycles without crashing, memory leaks, or failing to reconnect. |
| **Pass/Fail** | All 20 cycles complete. No need to power-cycle DUT. |
| **Equipment** | DUT, host device |

### STRESS-03: Controller Plug/Unplug During Operation

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT via BLE with controller attached. 2. Unplug Dreamcast controller cable. 3. Wait for "MAPLE: Controller lost" in RTT. 4. Plug controller back in. 5. Wait for "MAPLE: Controller re-detected". 6. Verify inputs work. 7. Repeat 10 times. |
| **Expected** | DUT detects loss after CONTROLLER_LOST_THRESHOLD (30 failures), re-detects when plugged back, resumes normal input reporting. |
| **Pass/Fail** | All 10 cycles succeed. No crashes, no stuck states. |
| **Equipment** | DUT, Dreamcast controller, SWD debugger |

### STRESS-04: Sustained High-Frequency Input

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT. 2. Continuously move analog stick in circles while pressing buttons randomly for 10 minutes. 3. Monitor RTT for errors. |
| **Expected** | No buffer overflows, no missed reports, no crashes. Reports continue at ~125Hz. |
| **Pass/Fail** | 10 minutes continuous operation with no errors in RTT log. |
| **Equipment** | DUT, Dreamcast controller, host, SWD debugger |

### STRESS-05: Power Cycle Endurance (50 Cycles)

| Field | Value |
|-------|-------|
| **Steps** | 1. Pair DUT with host. 2. Remove and reinsert battery 50 times (or toggle power switch if available). 3. Verify reconnection works after final cycle. |
| **Expected** | Bond persists across all power cycles. Flash does not corrupt. Device boots and reconnects every time. |
| **Pass/Fail** | Successful reconnection after cycle 50 with no flash corruption. |
| **Equipment** | DUT, LiPo with accessible connector |

### STRESS-06: Rapid Name Toggle

| Field | Value |
|-------|-------|
| **Steps** | 1. Triple-press sync 10 times (alternating Xbox/Dreamcast). 2. After each toggle, verify correct name on host scan. |
| **Expected** | Flash name preference toggles correctly each time. No flash corruption. DUT resets and boots cleanly each time. |
| **Pass/Fail** | All 10 toggles produce correct name. |
| **Equipment** | DUT, host device |

---

## 5. Compatibility Testing

### COMPAT-01: iBlueControlMod Receiver

| Field | Value |
|-------|-------|
| **Steps** | 1. Ensure Xbox name mode. 2. Pair DUT with iBlueControlMod. 3. Connect to Dreamcast console. 4. Test all inputs in a game. |
| **Expected** | iBlueControlMod recognizes device by name, parses HID descriptor correctly. All inputs map to correct Dreamcast functions. |
| **Pass/Fail** | All inputs functional in a Dreamcast game through the receiver. |
| **Equipment** | DUT, Dreamcast controller, iBlueControlMod, Dreamcast console, game disc |

### COMPAT-02: Windows 10/11

| Field | Value |
|-------|-------|
| **Steps** | 1. Pair DUT with Windows PC via Bluetooth. 2. Open "Set up USB game controllers" (joy.cpl). 3. Test all inputs. |
| **Expected** | Windows recognizes device as Xbox controller or generic gamepad. All axes and buttons register in calibration tool. |
| **Pass/Fail** | Device appears in Game Controllers. All inputs functional. |
| **Equipment** | DUT, Dreamcast controller, Windows PC with Bluetooth |

### COMPAT-03: macOS

| Field | Value |
|-------|-------|
| **Steps** | 1. Pair DUT with Mac via Bluetooth. 2. Open a game or gpadtester.com in browser. 3. Test all inputs. |
| **Expected** | macOS pairs and recognizes gamepad. Inputs register in Gamepad API. |
| **Pass/Fail** | Pairing succeeds, all inputs register. |
| **Equipment** | DUT, Dreamcast controller, Mac with Bluetooth |

### COMPAT-04: iOS (iPhone/iPad)

| Field | Value |
|-------|-------|
| **Steps** | 1. Pair DUT with iOS device. 2. Open a MFi/Game Controller-compatible game. 3. Test inputs. |
| **Expected** | iOS recognizes as game controller. Inputs functional. Note: iOS may require specific HID report format compliance. |
| **Pass/Fail** | Pairing succeeds, inputs register in at least one game. |
| **Equipment** | DUT, Dreamcast controller, iPhone or iPad |

### COMPAT-05: Android

| Field | Value |
|-------|-------|
| **Steps** | 1. Pair DUT with Android device. 2. Open a gamepad-compatible game or Gamepad Tester app. 3. Test inputs. |
| **Expected** | Android recognizes as gamepad. All inputs map correctly. |
| **Pass/Fail** | Pairing succeeds, all inputs register. |
| **Equipment** | DUT, Dreamcast controller, Android phone |

### COMPAT-06: Linux (Steam Deck / Desktop)

| Field | Value |
|-------|-------|
| **Steps** | 1. Pair DUT with Linux system. 2. Check `jstest /dev/input/jsX` or `evtest`. 3. Test inputs. |
| **Expected** | Linux HID driver recognizes device. xpadneo or generic HID driver picks it up. All axes and buttons visible. |
| **Pass/Fail** | Device enumerated, all inputs functional. |
| **Equipment** | DUT, Dreamcast controller, Linux system with Bluetooth |

### COMPAT-07: Nintendo Switch (via Bluetooth)

| Field | Value |
|-------|-------|
| **Steps** | 1. Attempt to pair DUT with Nintendo Switch via "Change Grip/Order". |
| **Expected** | Document result. Switch may or may not recognize Xbox HID format. |
| **Pass/Fail** | Informational -- document whether it pairs and which inputs work. |
| **Equipment** | DUT, Dreamcast controller, Nintendo Switch |

### COMPAT-08: Third-Party Dreamcast Controller

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect a third-party Dreamcast controller to DUT. 2. Pair and test all inputs. |
| **Expected** | Maple Bus protocol is standard. Third-party controllers should work if they implement the standard Get Condition response (cmd 0x08, func 0x01). |
| **Pass/Fail** | Device detected, all inputs register. Note any differences in range or behavior. |
| **Equipment** | DUT, third-party Dreamcast controller, host |

---

## 6. Edge Cases

### EDGE-01: Boot with No Battery (USB Only)

| Field | Value |
|-------|-------|
| **Steps** | 1. Remove LiPo battery. 2. Connect USB only. 3. Power on DUT. 4. Verify normal operation. |
| **Expected** | DUT boots from USB power. Battery reads 0mV or very low. Low battery cutoff should NOT trigger because charge STAT pin may read as charging. Boost remains off (USB passthrough). |
| **Pass/Fail** | DUT operates normally on USB power without battery. Does not enter System Off erroneously. |
| **Equipment** | DUT, USB cable (no battery) |

### EDGE-02: Boot with Controller Already Connected

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect Dreamcast controller to DUT before power-on. 2. Power on DUT. 3. Wait for BLE connection. 4. Verify controller detected immediately. |
| **Expected** | Controller detected on first or second probe after BLE connects. No extended search phase. |
| **Pass/Fail** | Controller detected within 2 seconds of BLE connection. |
| **Equipment** | DUT, Dreamcast controller, host |

### EDGE-03: BLE Disconnect During Controller Detection

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT via BLE (no controller attached). 2. DUT starts searching for controller. 3. Immediately disconnect BLE from host. |
| **Expected** | DUT aborts controller detection, disables boost, returns to advertising. RTT shows "MAIN: BLE disconnected during detection, disabling boost". |
| **Pass/Fail** | Clean state transition, no crash, boost disabled. |
| **Equipment** | DUT, host, SWD debugger |

### EDGE-04: BLE Disconnect During Active Gaming

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT fully (BLE + controller, playing). 2. Turn off host Bluetooth abruptly. 3. Observe DUT behavior. |
| **Expected** | DUT detects disconnect (GATT run returns), disables boost, resets controller state to default, returns to reconnecting state. |
| **Pass/Fail** | Clean transition, no crash, boost disabled, LEDs off. |
| **Equipment** | DUT, Dreamcast controller, host, SWD debugger |

### EDGE-05: Controller Disconnect During Active Gaming

| Field | Value |
|-------|-------|
| **Steps** | 1. Playing with full connection. 2. Unplug Dreamcast controller cable. 3. Observe DUT behavior. 4. Do NOT replug for 60+ seconds. |
| **Expected** | After 30 failed polls (CONTROLLER_LOST_THRESHOLD), DUT enters re-detection loop. After SLEEP_TIMEOUT_MS, DUT enters System Off. Default controller state (centered sticks, no buttons) sent to host. |
| **Pass/Fail** | Host receives neutral state. DUT eventually sleeps. No crash. |
| **Equipment** | DUT, Dreamcast controller, host, SWD debugger, stopwatch |

### EDGE-06: Double Button Press (Sync Debounce)

| Field | Value |
|-------|-------|
| **Steps** | 1. Quickly double-tap sync button (not triple). 2. Observe behavior. |
| **Expected** | Two short presses within 2 seconds should NOT trigger name toggle (requires 3 presses). Should not trigger sync mode (requires 3-second hold). |
| **Pass/Fail** | No state change from double-tap. |
| **Equipment** | DUT |

### EDGE-07: Hold Sync During Connected State

| Field | Value |
|-------|-------|
| **Steps** | 1. DUT connected and operating normally. 2. Hold sync button for 3 seconds. |
| **Expected** | Enters sync mode, clears bond, disconnects from current host, becomes discoverable. |
| **Pass/Fail** | Clean transition to sync mode. Old host cannot reconnect without re-pairing. |
| **Equipment** | DUT, host |

### EDGE-08: Simultaneous Sleep and BLE Activity

| Field | Value |
|-------|-------|
| **Steps** | 1. DUT connected. 2. Hold sync for 7+ seconds to trigger System Off. 3. While holding, verify the host sees disconnect. |
| **Expected** | DUT enters System Off. BLE connection drops. Host sees disconnection. |
| **Pass/Fail** | Clean shutdown. No corruption of bond data. |
| **Equipment** | DUT, host |

### EDGE-09: Bond Flash Page Wear

| Field | Value |
|-------|-------|
| **Steps** | 1. Run STRESS-02 (20 connect/disconnect cycles). 2. Each disconnect saves bond to flash. 3. Run STRESS-06 (10 name toggles, each writes flash). 4. Verify flash reads back correctly after all writes. |
| **Expected** | nRF52840 flash rated for 10,000 erase cycles per page. 30 writes is negligible. Bond and name data should read back correctly. |
| **Pass/Fail** | Bond loads correctly after test. Name preference loads correctly. |
| **Equipment** | DUT, SWD debugger |

### EDGE-10: Notify Failure Disconnect

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT. 2. Move host out of range or create heavy interference to cause notify failures. 3. Observe RTT for "BLE: Too many notify failures, disconnecting". |
| **Expected** | After MAX_NOTIFY_FAILURES consecutive failures, DUT intentionally disconnects and returns to reconnecting state. |
| **Pass/Fail** | Controlled disconnect occurs, DUT does not crash. Reconnects when host returns to range. |
| **Equipment** | DUT, host, SWD debugger, means to degrade BLE (distance or interference) |

### EDGE-11: Report During Service Discovery Window

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT. 2. Immediately start pressing buttons (before SERVICE_DISCOVERY_DELAY_MS elapses). 3. Continue pressing after delay. |
| **Expected** | Reports are not sent during discovery window (host may not have subscribed yet). After the delay, reports begin and inputs register. |
| **Pass/Fail** | No errors during discovery window. Inputs register after delay. |
| **Equipment** | DUT, Dreamcast controller, host, gpadtester.com |

### EDGE-12: Startup with Discharged Battery + USB

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect nearly dead LiPo (around 3.0V). 2. Also connect USB. 3. Power on. |
| **Expected** | USB provides operating power. Charge STAT = charging. Low battery cutoff inhibited while charging. DUT operates normally while LiPo charges. |
| **Pass/Fail** | DUT boots and operates, does not enter low-battery shutdown. |
| **Equipment** | DUT, low-voltage LiPo, USB cable |

---

## 7. Latency Testing

### LAT-01: Input-to-BLE Latency (Estimated)

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT. 2. Calculate theoretical latency: Maple Bus poll (16ms) + BLE notify interval (8ms) + connection interval (8.75-11.25ms). 3. Optionally: record screen at 240fps while pressing a button with a visible indicator (LED or on-screen), measure frames between physical press and screen update. |
| **Expected** | Theoretical worst case: 16 + 8 + 11.25 = ~35ms one-way. With host processing and display, end-to-end < 60ms. |
| **Pass/Fail** | Subjectively, input should feel responsive with no perceptible lag during gameplay. |
| **Equipment** | DUT, Dreamcast controller, host, high-speed camera (optional) |

### LAT-02: Polling Rate Verification

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect DUT. 2. Monitor RTT output timing for Maple Bus polls (every POLL_INTERVAL_MS = 16ms). 3. Enable HID report notifications in nRF Connect, observe notification rate. |
| **Expected** | Maple Bus polls at ~60Hz. BLE notifications at ~125Hz. |
| **Pass/Fail** | Poll rate within 10% of target. Notification rate within 10% of target. |
| **Equipment** | DUT, SWD debugger, nRF Connect app |

---

## 8. Safety and Robustness

### SAFE-01: Panic Recovery (panic-reset)

| Field | Value |
|-------|-------|
| **Steps** | 1. Verify `panic_reset` is the panic handler in the build. 2. (If possible) Trigger a panic condition. 3. Observe DUT resets and boots cleanly. |
| **Expected** | On panic, DUT resets rather than halting. Boots fresh, reconnects to bonded host. |
| **Pass/Fail** | Recovery within 5 seconds. |
| **Equipment** | DUT, SWD debugger |

### SAFE-02: Thermal Check (Extended Use)

| Field | Value |
|-------|-------|
| **Steps** | 1. Run DUT for 2 hours on battery with boost converter active. 2. Periodically touch boost converter, LiPo, and XIAO board. |
| **Expected** | Components warm but not hot. Boost converter should be < 50C. LiPo should not be warm unless charging. |
| **Pass/Fail** | No component uncomfortably hot to touch (> 50C). |
| **Equipment** | DUT, LiPo, finger (or IR thermometer if available) |

### SAFE-03: Reverse Polarity Protection

| Field | Value |
|-------|-------|
| **Steps** | (Informational) Verify circuit design includes reverse polarity protection on battery input, or document that the JST connector prevents reverse insertion. |
| **Expected** | Document protection method. |
| **Pass/Fail** | Informational. |
| **Equipment** | Circuit documentation |

### SAFE-04: Charging Safety -- Over-Temperature

| Field | Value |
|-------|-------|
| **Steps** | 1. Connect USB to charge LiPo. 2. Monitor LiPo temperature during charging. 3. Verify BQ25101 terminates charge if battery gets warm. |
| **Expected** | BQ25101 handles charge termination. LiPo should not exceed 45C during normal charging. 100mA charge rate is very conservative for most cells. |
| **Pass/Fail** | Battery stays cool (< 40C) during charging at 100mA. |
| **Equipment** | DUT, USB cable, LiPo, thermometer (optional) |

---

## Test Execution Tracking

| Test ID | Date | Result | Notes |
|---------|------|--------|-------|
| FUNC-01 | | | |
| FUNC-02 | | | |
| FUNC-03 | | | |
| FUNC-04 | | | |
| FUNC-05 | | | |
| FUNC-06 | | | |
| FUNC-07 | | | |
| FUNC-08 | | | |
| FUNC-09 | | | |
| FUNC-10 | | | |
| FUNC-11 | | | |
| BLE-01 | | | |
| BLE-02 | | | |
| BLE-03 | | | |
| BLE-04 | | | |
| BLE-05 | | | |
| BLE-06 | | | |
| BLE-07 | | | |
| BLE-08 | | | |
| BLE-09 | | | |
| BLE-10 | | | |
| BLE-11 | | | |
| BLE-12 | | | |
| BLE-13 | | | |
| BLE-14 | | | |
| BLE-15 | | | |
| BLE-16 | | | |
| PWR-01 | | | |
| PWR-02 | | | |
| PWR-03 | | | |
| PWR-04 | | | |
| PWR-05 | | | |
| PWR-06 | | | |
| PWR-07 | | | |
| PWR-08 | | | |
| PWR-09 | | | |
| PWR-10 | | | |
| PWR-11 | | | |
| PWR-12 | | | |
| PWR-13 | | | |
| PWR-14 | | | |
| PWR-15 | | | |
| PWR-16 | | | |
| STRESS-01 | | | |
| STRESS-02 | | | |
| STRESS-03 | | | |
| STRESS-04 | | | |
| STRESS-05 | | | |
| STRESS-06 | | | |
| COMPAT-01 | | | |
| COMPAT-02 | | | |
| COMPAT-03 | | | |
| COMPAT-04 | | | |
| COMPAT-05 | | | |
| COMPAT-06 | | | |
| COMPAT-07 | | | |
| COMPAT-08 | | | |
| EDGE-01 | | | |
| EDGE-02 | | | |
| EDGE-03 | | | |
| EDGE-04 | | | |
| EDGE-05 | | | |
| EDGE-06 | | | |
| EDGE-07 | | | |
| EDGE-08 | | | |
| EDGE-09 | | | |
| EDGE-10 | | | |
| EDGE-11 | | | |
| EDGE-12 | | | |
| LAT-01 | | | |
| LAT-02 | | | |
| SAFE-01 | | | |
| SAFE-02 | | | |
| SAFE-03 | | | |
| SAFE-04 | | | |

---

## References and Tools

- [Gamepad Tester (gpadtester.com)](https://gpadtester.com/) -- browser-based HID gamepad input visualizer
- [nRF Connect (Nordic)](https://www.nordicsemi.com/Products/Development-tools/nRF-Connect-for-mobile) -- BLE service browser and debugger
- [HIDViz](https://github.com/hidviz/hidviz) -- USB HID protocol analyzer (deep descriptor inspection)
- [HIDAPI](https://github.com/libusb/hidapi) -- cross-platform HID communication library with test GUI
- [ControllersInfo](https://github.com/DJm00n/ControllersInfo) -- reference HID descriptors for Xbox and other controllers
- [Linux HID gamepad selftests](https://github.com/torvalds/linux/blob/master/tools/testing/selftests/hid/tests/test_gamepad.py) -- Linux kernel HID gamepad test framework
- [Bluetooth Qualification](https://www.bluetooth.com/develop-with-bluetooth/qualify/) -- Bluetooth SIG qualification process
- [IEC 62133 (Battery Safety)](https://www.intertek.com/batteries/iec-62133/) -- international standard for lithium battery safety
- [Punch Through BLE Connection Guide](https://punchthrough.com/manage-ble-connection/) -- comprehensive BLE connection parameter reference
- [Nordic Developer Academy -- BLE Connections](https://academy.nordicsemi.com/courses/bluetooth-low-energy-fundamentals/lessons/lesson-3-bluetooth-le-connections/topic/connection-parameters/) -- connection parameter tuning guide

### Bluetooth Qualification Notes

For a product sold commercially, Bluetooth SIG qualification is required. Key points:
- The nRF52840 SoftDevice S140 is already qualified as a Bluetooth subsystem (QDID from Nordic).
- An end-product listing is still needed, referencing the SoftDevice QDID.
- HOGP (HID over GATT Profile) compliance should be verified against the Bluetooth SIG TCRL.
- Cost for end-product listing is typically low when using a pre-qualified subsystem.

### IEC 62133 / Battery Safety Notes

For a product with an integrated LiPo battery sold commercially:
- IEC 62133-2:2017 covers secondary lithium cells and batteries for portable applications.
- Tests include: external short circuit, thermal abuse, crush, overcharge, forced discharge.
- The BQ25101 charge IC provides charge termination, but the overall product must be assessed.
- For a hobby/prototype, focus on: no exposed battery terminals, charge current within cell rating, low-voltage cutoff implemented (PWR-09).
