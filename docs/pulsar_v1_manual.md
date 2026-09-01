# Pulsar v1 — Owner's Manual

Your Pulsar is a Dreamcast controller adapter. It reads a real Dreamcast controller over
the Maple Bus and presents it to your computer, phone, or console as a standard Bluetooth
gamepad. No dongle, no drivers.

This manual covers **Pulsar v1** — the assembled unit in the VMU-shaped shell. If you built
your own on a XIAO or an nRF52840-DK, see [`users_guide.md`](users_guide.md) instead, which
covers all three boards.

---

## 1. Connecting your controller

Pulsar ships in one of two ways. Check which one you have before going further.

### If your unit came with a controller

It is already wired. Plug the adapter's lead into the controller if it isn't already, and
skip to [First-time pairing](#2-first-time-pairing).

### If you bought the adapter on its own

Your unit ships with a 5-pin JST lead fitted to its **CN1** port, with bare flying leads at
the far end. You supply the Dreamcast controller and join the two. **This is a soldering
job and it permanently modifies the controller** — the cable gets cut. Budget half an hour,
and use a controller you're willing to alter.

You will need: a multimeter, a soldering iron, wire strippers, heat-shrink or tape.

**Step 1 — cut the controller cable.** Cut it a comfortable working distance from the
controller, keeping the length you want between the controller and the adapter. Keep the
controller end; the plug end is not used.

**Step 2 — identify the five conductors with a meter.** The cable carries five conductors
plus a shield. The Maple pinout is:

| Pin | Typical color | Function |
|-----|---------------|----------|
| 1 | Red | SDCKA — data/clock A |
| 2 | Blue | +5 V power |
| 3 | Black | Ground (the cable shield ties here) |
| 4 | Green | Sense — tied to ground |
| 5 | White | SDCKB — data/clock B |

> **Meter your actual cable. Do not trust the colors.** Third-party and later-revision
> Dreamcast controllers use different color codes, and the table above is the canonical
> Maple pinout, not a promise about the cable in your hand. Buzz each conductor from the
> cut end back to the controller's connector to establish which is which.

**Step 3 — splice.** Join each conductor to the matching wire on the adapter's lead.
Insulate every joint individually, then sleeve the bundle. Keep the splice slim — it has to
sit inside the shell without straining the cable exit.

**Step 4 — check before you power it.** With nothing plugged in, check continuity end to
end on all five conductors, and check for shorts *between* adjacent conductors. A short
between +5 V and ground, or between either data line and power, can damage the adapter or
the controller. This check takes two minutes and is worth every one of them.

**Step 5 — plug into CN1** and pair. If the controller is never detected, come back to
[Troubleshooting](#11-troubleshooting).

---

## 2. First-time pairing

**Pair first, then the controller wakes up.** This is the reverse of most adapters and it
is the single most common source of confusion, so it's worth reading before you start.

Pulsar does not power the controller until a Bluetooth host is connected. An adapter
sitting on your desk plugged into a charger spends that energy on its battery instead of
running a controller nobody is using. So until you pair, **the controller is unpowered and
a docked VMU is blank. That is normal, not a fault.**

1. **Wake the adapter** with a short press of the sync button.
2. On first power-on it enters **pairing mode automatically** (nothing is bonded yet) and
   stays discoverable for 60 seconds. If it has been paired before, hold sync for
   **2 seconds** to enter pairing mode.
3. On your host, open Bluetooth settings and look for **"Xbox Wireless Controller."**
4. Select it to pair.
5. The controller comes alive as the connection completes — a docked VMU chirps and flashes
   as it boots, and the status LED turns green.

The adapter remembers your host and reconnects on its own from then on. Just press sync to
wake it.

**Pulsar holds one pairing at a time**, like an Xbox controller. Pairing to a new host
means the old one still lists the adapter and won't reconnect to it — forget the adapter
there before pairing it back.

---

## 3. Button mapping

| Dreamcast | Sends as |
|-----------|----------|
| A | A |
| B | B |
| X | X |
| Y | Y |
| Start | Menu |
| D-pad (all 8 directions) | D-pad |
| Left trigger | Left trigger (analog) |
| Right trigger | Right trigger (analog) |
| Analog stick | Left stick |

The Dreamcast pad has one analog stick, so there is no right stick.

### The Guide button

The Dreamcast pad has no Guide button, so it's a chord: **pull both triggers and hold Start
for about a third of a second.**

This opens the Steam overlay, Big Picture, or the Xbox Game Bar, depending on your host.
It's deliberately a hold rather than a tap so it can't fire mid-game, and it deliberately
avoids the face buttons so it never collides with the Dreamcast's own
`A+B+X+Y+Start` soft-reset combo. While you hold it, the triggers and Start are suppressed,
so the host sees only Guide.

On the **Dreamcast** profile the same chord arrives as plain **Button 11** — bind that to
Guide in Steam Input if you want the overlay there.

---

## 4. The sync button

One button, five gestures. The LED blinks while you hold, and **blinks faster once you pass
2 seconds**, so you can see the next step coming and release in time.

| Gesture | What happens |
|---|---|
| **Short press** | Wake, or ask to reconnect |
| **Hold 2 s** | Pairing mode for 60 s — clears the current pairing |
| **Triple-press** | Switch profile (Xbox ⇄ Dreamcast), then reboot |
| **Hold 3.5 s, with the controller's Start held** | Firmware update mode |
| **Tap, tap, then hold 3.5 s** | Firmware update mode, no controller needed |
| **Tap once, then hold 3.5 s** | [Browser configuration mode](#remapping-your-buttons) — remap buttons |
| **Hold 7 s** | Sleep — shows `BYE`, then powers down |

Entering update mode does **not** clear your pairing. Only the 2-second hold does that.

The three tap-then-hold gestures differ only in **how many taps come before the hold**: one
tap is configuration, two is the controller-free update, and three short presses with no
hold is the profile toggle. Start the hold within about two seconds of the first tap.

### Profiles

Two identities, switched with a triple-press.

| Profile | Advertises as | Use it for |
|---|---|---|
| **Xbox** (default) | Xbox Wireless Controller | Almost everything — macOS, Windows, Steam, Linux (xpadneo), BlueRetro, BLE receivers |
| **Dreamcast** | Dreamcast Wireless Controller | A plain generic HID gamepad, for hosts where you'd rather not appear as an Xbox pad |

**Use Xbox unless you have a reason not to.** The Dreamcast profile is *not* Dreamcast
controller emulation — it's an ordinary generic BLE gamepad, and only the Bluetooth name is
Dreamcast-branded. Pick it to avoid Xbox button prompts in games, or for a host that
prefers a neutral HID device.

After switching, **forget the adapter on your host and pair again** — most hosts cache the
button layout against the old pairing. The choice is saved and survives a restart.

### Remapping your buttons

**Firmware 243 or later.** You can change what every control sends, from a browser, with
nothing to install. The map is stored on Pulsar itself, applies to **both profiles**, and
travels with the controller to every host you pair with.

**Tap sync once, then press it again and hold through 3.5 seconds.** Pulsar resets,
disconnects from your host and advertises as **`Pulsar Configure`** for 60 seconds. Open
<https://pulsar.alwaysagog.com/configure> in **Chrome or Edge on a computer, or Chrome on
Android**, and connect to it there. Safari and Firefox cannot do this — they do not support
Web Bluetooth.

Press a control and you will see it register live. Changes preview on the adapter before
you commit them, so you can feel a layout before keeping it — then **save to adapter** to
make it stick.

> Nothing is written until you save, so if the session ends mid-preview — you disconnect,
> the 60 seconds lapse, or the battery dies — Pulsar goes back to the last map you saved,
> with your pairing untouched.

---

## 5. The LED bar

Five LEDs behind the shell window. The first carries status; the other four are the battery
gauge. They're deliberately dim to avoid glare.

| LED 0 | Meaning |
|---|---|
| Blue sweep across the bar | Starting up |
| Dim red | Looking for the controller |
| Dim green | Controller connected |
| Blue, blinking | Sync button held, or in pairing mode |
| Blue, blinking faster | You're past 2 seconds — keep holding and it sleeps |
| Five quick flashes | Profile switched, rebooting |
| All dark | Asleep, or idle and waiting for a host |

**LEDs 1–4 are the battery gauge**, in magenta — a different color from the status LED so
the two can't be confused. They stay lit through status changes, so losing the controller
doesn't blank your battery reading.

A lit bar means the battery is connected. It does not tell you the controller has power —
that only happens once a host connects.

---

## 6. The VMU display

Dock a VMU and the adapter draws on it while connected: a profile splash when you connect,
holding about 30 seconds, then a rotating pulsar with a battery indicator. Pairing shows
`SYNC`; going to sleep shows `BYE`.

<table>
  <tr>
    <td align="center"><img src="images/std.jpeg" width="200" alt="Xbox profile splash"><br>Xbox profile</td>
    <td align="center"><img src="images/ext.jpeg" width="200" alt="Dreamcast profile splash"><br>Dreamcast profile</td>
    <td align="center"><img src="images/pulsar.jpeg" width="200" alt="Rotating pulsar"><br>In session</td>
  </tr>
  <tr>
    <td align="center"><img src="images/sync.jpeg" width="200" alt="SYNC splash"><br>Pairing</td>
    <td align="center"><img src="images/bye.jpeg" width="200" alt="BYE splash"><br>Sleeping</td>
    <td></td>
  </tr>
</table>

A dark VMU tells you nothing on its own — a powered VMU that nobody has written to looks
exactly like an unpowered one. The chirp on connect is the reliable sign it has power.

Animating the display costs roughly 5–10% of battery life against running with no VMU
docked.

---

## 7. Battery and charging

Charge over **USB-C** on the adapter itself. A phone charger or USB power brick is ideal.

The gauge reports in **four steps — 25 / 50 / 75 / 100%** — shown on LEDs 1–4 and on the
VMU, and reported to your host over Bluetooth (so it appears in your Bluetooth settings and
in games that show controller battery). It moves in visible jumps rather than sliding down
gradually. That's the gauge's resolution, not a fault.

> **Some laptops won't charge it, especially MacBooks.** A laptop port that looks for a USB
> device may cut power to a sleeping adapter, which has its USB peripheral switched off. If
> charging seems not to happen from a laptop, use a wall charger, or keep the adapter awake
> while it's plugged in.

**While it is charging, the VMU's battery icon shows a lightning bolt instead of the level
bars.** That is not a fifth level — it replaces the reading, and the level comes back the
moment you unplug. A charger holds the battery's voltage up, so any level measured while
charging reads high; the bolt says the number would be misleading rather than showing you a
wrong one. If the icon is completely empty, charge it now.

---

## 8. Sleep and wake

The adapter sleeps to save battery when:

- you hold sync for **7 seconds**;
- **60 seconds** pass after power-on with no Bluetooth connection;
- **60 seconds** pass with Bluetooth connected but no controller detected;
- **60 seconds** pass after the controller is disconnected while Bluetooth is connected;
- **10 minutes** pass with no input from the controller while connected.

**To wake it, press sync.** It restarts and reconnects to your paired host.

Charging works normally while it sleeps. If you're storing it for a while, charge it first
and expect to top it up before the next session.

---

## 9. Updating the firmware

Pulsar v1 updates **wirelessly, from a web page** — no cables, no drivers, no software to
install.

1. Charge the adapter, or plug it in. Updates are refused **below 50% battery** unless it's
   charging; the VMU shows `CHRG` if so.
2. Enter update mode: hold sync past **3.5 seconds while holding the controller's Start**.
   Three fast flashes confirm it. No controller attached? **Tap sync twice, then hold past
   3.5 seconds**, starting the hold within about two seconds of the first tap.
3. The adapter reboots and advertises as **`PulsarDFU`** — not its usual name.
4. Open the update page in a supported browser and follow it through.

Updates are signed, so the adapter only accepts official firmware.

### Your browser has to support Web Bluetooth

If the update page never shows a device chooser, this is almost always why.

| Browser | Works? |
|---|---|
| **Chrome / Edge / Opera** on Windows or macOS | Yes |
| **Brave** | Yes, but you must enable it first — `brave://flags` → **Web Bluetooth API** → Enabled → relaunch |
| **Chrome on Linux** | Needs `chrome://flags/#enable-experimental-web-platform-features`, plus BlueZ 5.41+ |
| **Firefox** | No — unsupported on every platform |
| **Safari**, including iPhone and iPad | No — Apple does not implement the API |

On an iPhone or iPad there is no way to make Safari work. Update from a computer, or use a
Web Bluetooth browser such as Bluefy.

> **There is no reset button, and no USB drive.** Double-tapping anything does nothing, and
> no drive will appear on your computer. The sync gestures above are the only way into
> update mode — this is deliberate.
>
> **An interrupted update is not a brick.** If the firmware ends up missing or incomplete,
> the adapter enters update mode by itself at power-on and waits. Plug it in, reload the
> page, try again.

---

## 10. What this unit does not do

- **No rumble.** The board can drive a rumble motor, but **no motor is fitted** to units at
  this revision. Rumble commands from your host arrive and are ignored, harmlessly.
- **No right stick.** The Dreamcast controller doesn't have one.
- **One host at a time.** Pairing to a new device replaces the old pairing.

---

## 11. Troubleshooting

**The controller and VMU are dead until I connect a host**
Expected — see [First-time pairing](#2-first-time-pairing). The adapter powers the
controller only once a Bluetooth host connects. Connect your host: the VMU chirps and the
status LED goes green. If it stays dark *after* connecting, keep reading.

**The status LED stays red — the controller is never found**
Check the lead is fully seated at both ends. On a self-wired unit, re-check your splice:
continuity end to end on all five conductors, and no shorts between them. A miswired data
line looks exactly like a dead controller.

**I can't find "Xbox Wireless Controller" in Bluetooth**
Hold sync for 2 seconds to force pairing mode — it's only discoverable for 60 seconds at a
time. Stay within about 10 m. If this host paired with it before, forget the adapter there
first, then pair fresh.

**It won't reconnect after I paired it to something else**
Pulsar holds one pairing. Forget it on the device you're trying to return to, then hold sync
2 seconds and pair again.

**Connected, but no input**
Give the host a few seconds after connecting to finish discovering the controller. Then
press a button on the Dreamcast pad to confirm it's responding.

**The buttons are mapped wrong**
Try the other profile — triple-press sync — then forget the adapter on your host and pair
fresh, since most hosts cache the layout. Also check whether your host is remapping things
itself (Steam Input, accessibility shortcuts).

**Flycast's mapping screen leaves the stick "Up"/"Left" rows blank**
Cosmetic. Use the Xbox profile for SDL-based emulators. After "Reset to default," Flycast
labels only the Down/Right rows but binds the *full* axis to both directions — the stick
works fully in game. Don't hand-map the blank rows.

**The battery indicator jumps in big steps**
Normal — the gauge has four levels, so it steps 100 → 75 → 50 → 25.

**It keeps going to sleep**
See [Sleep and wake](#8-sleep-and-wake) for the five timeouts. Press sync to wake it.

**The update page never finds the adapter**
Check your browser first — see the table in [Updating the
firmware](#9-updating-the-firmware); Brave needs a flag and Firefox and Safari can't do it
at all. Then confirm the adapter is actually in update mode: it advertises as `PulsarDFU`,
not under its usual name.

---

## Reference

- [`users_guide.md`](users_guide.md) — the full guide, covering the DIY XIAO and DK builds too
- [`pin_mapping.md`](pin_mapping.md) — complete pinouts for every board
- [`maple_bus_protocol.md`](maple_bus_protocol.md) — how the Dreamcast controller is actually read
