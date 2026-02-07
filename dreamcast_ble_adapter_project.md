# 🕹️ Dreamcast Maple Bus Bluetooth Adapter  
**nRF52840-based embedded project to bridge genuine Dreamcast controllers to PC and Dreamcast over BLE**

---

## 🧩 Overview
This embedded Rust project enables a real Sega Dreamcast controller to communicate wirelessly over Bluetooth using a Nordic **nRF52840** development kit. It interfaces with the Dreamcast's proprietary **Maple Bus** protocol and supports future expansion for PC host mode and BLE-based passthrough.

The goal is to **retain original hardware feel** while enabling **modern wireless use** and **reverse-engineering tooling**.

---

## 📦 Hardware
- **Target MCU**: Nordic **nRF52840 DK** (ARM Cortex-M4F, single core)
- **Power**: LiPo battery (with onboard charger)
- **BLE**: Nordic softdevice stack (planned)
- **Dreamcast interface**: 3.3V GPIO connection to Maple Bus lines
- **Debugging**: RTT via SWD + `cargo-embed`

---

## 🧱 Software Stack

| Layer               | Choice                              |
|--------------------|--------------------------------------|
| MCU support        | `nrf52840-hal`, `nrf52840-dk-bsp`    |
| Embedded runtime   | `cortex-m`, `cortex-m-rt`            |
| Panic handler      | `panic-halt`                         |
| Logging            | `rtt-target` (replaced `defmt`)      |
| Timing             | `Timer<TIMER0, OneShot>` via HAL     |
| GPIO               | BSP HAL buttons/LEDs                 |
| Protocol logic     | Custom `MapleBusTrait` and `MapleController` abstractions |
| Abstraction        | `heapless::Vec`, modular `traits.rs` |
| Unit Testing       | `MockMapleBus` implementation        |

---

## 🧠 Architecture

- **`maple/` module**
  - `packet.rs`: low-level encoding/decoding of Maple packets
  - `bus.rs`: MapleBus state machine and I/O abstraction
  - `mock_bus.rs`: simulated MapleBus for logic testing
  - `state_machine.rs`: `MapleController` for high-level flow
  - `traits.rs`: defines `MapleBusTrait` for pluggable transport
- **`main.rs`**
  - Entry point
  - GPIO + timer setup
  - Prototype input detection and LED blinking
  - Flash/test via `cargo embed`

---

## ✅ Current Features

- [x] Mock MapleBus state machine tested in `main.rs`
- [x] Blinks LED on button press (tested on real hardware)
- [x] RTT debug logs using `rtt-target`
- [x] Modular structure for easy hardware simulation and swapping
- [x] Boot and timer setup on `nRF52840-DK`
- [x] **Maple Bus TX** - Send packets via GPIO bit-banging
- [x] **Maple Bus RX** - Bulk sampling decoder with phase alignment
- [x] **Device Info Request/Response** - Full transaction with CRC verification
- [x] Start pattern detection (handles false starts)
- [x] Static 96KB sample buffer for reliable 2Mbps capture

---

## 🔜 Next Goals

### Maple Bus - Controller Input
- [ ] Get Condition (0x09) - read buttons/sticks
- [ ] 60Hz polling loop
- [ ] Parse analog triggers and joystick values

### BLE Integration
- [ ] BLE softdevice initialization
- [ ] GATT service for controller state
- [ ] Expose buttons/sticks as BLE characteristics
- [ ] BLE pairing on button hold

### Optional / Future
- [ ] PC HID over BLE or USB
- [ ] VMU support (read/write memory card)
- [ ] Rumble pack support

---

## 🧪 Debugging Setup

- **Flash:** via `cargo-embed`
- **RTT logs:** via `rtt-target` + `cargo embed openocd`
- **Visual output:** onboard LED indicators
- **Button interaction:** DK buttons (e.g. BTN1 to start pairing)

---

## 📓 Design Considerations

- Avoid `defmt` for now due to linker/RTT symbol conflicts with `rtt-target`
- Stick to single-core critical-section features (valid for Cortex-M4)
- Modular design enables mocking/testing and eventual HAL swap
- `heapless` used to support no_std, deterministic stack-allocated buffers
- **Bulk sampling approach** for RX - capture all GPIO samples, decode later
- **Static buffers** to avoid stack allocation delays in timing-critical code
- No interrupts needed - polling loop with bulk sampling handles 2Mbps reliably
