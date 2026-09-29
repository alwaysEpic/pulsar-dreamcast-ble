// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use pulsar_dreamcast_ble::ble::{get_connection_state, ConnectionState};
use pulsar_dreamcast_ble::maple::host::MapleResult;
#[cfg(feature = "spim-capture")]
use pulsar_dreamcast_ble::maple::spim_capture::SpimCapture;
use pulsar_dreamcast_ble::maple::{ControllerState, MapleBus, MapleHost};
use pulsar_dreamcast_ble::{ble, board, RAW_CONTROLLER_STATE};

use embassy_time::Instant;
use nrf_softdevice::Softdevice;
// Panic handler is registered via #[panic_handler] in pulsar_dreamcast_ble::panic_handler
use pulsar_dreamcast_ble::SLEEP_TIMEOUT_MS;
use pulsar_dreamcast_ble::{log, log_init};
use static_cell::StaticCell;

use pulsar_dreamcast_ble::BATTERY_LEVEL;

/// Poll-loop pacing. Current design (read the whole story below — it was
/// earned the hard way): a minimum spacing sleep, then every Maple
/// transaction starts at the head of a radio-quiet window
/// (`align_to_quiet_window`), which locks one poll to each BLE connection
/// event. The sections below document, in order, the three designs and the
/// field data that killed the first two.
///
/// # Why absolute, not relative (the layout lottery, strike three)
///
/// The previous design slept a relative 5ms at the bottom of the loop, so the
/// period was `body + 5ms` and everything that moved the body moved the
/// period — and, worse, moved the *phase* of every subsequent poll against
/// the connection-event clock. `get_condition` retries cost ~4-5ms each and
/// happen exactly when the Maple TX+capture window collides with radio
/// activity, so body time fed back into collision probability: a coupled
/// oscillator. Rebuilds of identical source shifted the base body by
/// microseconds and rolled the system between a fast-sweeping phase (1-4%
/// doubled conn intervals) and a dwelling one (up to 30%) — the 2026-08
/// post-OTA "regression" that took a day of exact-binary A/B to pin (see
/// the 2026-08-05 board bring-up measurements; the good and bad
/// binaries are instruction-identical in the whole RX/decode path, icache
/// off — the difference was never the code, it was the timing map).
///
/// Anchoring cuts the feedback wire: a retry-lengthened iteration eats its
/// own slack instead of shifting every later poll, and layout variance in
/// the body disappears into the sleep as long as the body fits the budget.
/// A body that does NOT fit is counted in `POLL_OVERRUNS` (readable via the
/// `poll-period-debug` HID channel, tag 0xB5) — a bad roll now flags itself
/// on-device in seconds instead of needing a day of captures.
///
/// # Why 13
///
/// - Healthy body is ~8.5ms (TX ~0.4 + capture ~3.1 + decode + VMU/misc),
///   leaving ~4.5ms of slack for retries and layout rolls.
/// - The blessed v209 layout measured a ~13.4ms emergent period across every
///   validated capture — 13 preserves the proven dynamics as a designed
///   constant instead of an accident.
/// - 13 < 15 strictly: the maximum age of the freshest sample at any
///   connection event is 13ms, so no event goes empty during motion. The
///   13:15 beat sweeps the full phase every ~7.5 polls — no dwell, no lock.
/// - (Pre-existing caveat, unchanged from the old design: a central that
///   grants the requested 11.25ms interval out-runs this period. The bench
///   host runs 15ms; revisit if a sub-13ms-interval host ever matters.)
///
/// # Why the anchor alone was not enough (field data, 2026-08-05 runs #40-43)
///
/// The fixed 13ms deadline cut the feedback only while the body fit the
/// budget. Measured on hardware: one collision retry costs ~5-7ms on top of
/// a ~4.5-6ms base body, so a colliding poll overruns any budget that fits
/// under the 15ms interval, and the overrun resync re-couples body time to
/// phase — v214's roll ran gc mean 17.5ms, 1.33 retries/poll, ~37
/// overruns/s, 27% doubled intervals *with the anchor active*. Anchored
/// rolls that mostly fit (v211) held 6-9.6%: better, still out of band.
///
/// # The actual fix: start polls where the radio isn't
///
/// The SoftDevice tells us when radio activity begins and ends
/// (`maple::radio_notify`, 800µs advance warning). The pacer below sleeps a
/// minimum spacing, then waits for a **fresh radio-INACTIVE edge** before
/// letting the next Maple transaction start — so the ~3.5ms TX+capture
/// window opens at the head of the ~12ms inter-event quiet gap with ~7ms of
/// margin, and collisions (hence retries, hence the entire phase-feedback
/// mechanism) are structurally absent instead of absorbed. The cadence
/// locks to one poll per connection event (~15ms → the 66.6Hz / IQR 0.9ms
/// blessed profile), and layout-independence is total: no plausible codegen
/// roll spans a 7ms margin.
///
/// # History: the 2026-07-24 knife-edge
///
/// The old relative delay was 5 and not 8 because at 8 the emergent period
/// sat at ~15ms — exactly the interval — and sub-millisecond codegen noise
/// decided which side of the line each build landed on (53.1Hz/IQR 14.6 vs
/// 66.9Hz/IQR 1.2 from identical code). That was this same coupled-oscillator
/// failure observed through a smaller window.
///
/// **Do not change these without a hardware capture.** Healthy is ~66.6Hz /
/// median 15.0ms / IQR ~0.9ms / (mean−median)/median within 1.3-4.0%.
/// Nothing in `ci.sh` detects the difference.
///
/// Nominal poll period (event-locked to the connection interval); used to
/// convert poll counts to durations (VMU splash/home holds, detect delay).
const POLL_PERIOD_MS: u64 = 15;

/// Minimum spacing between poll starts. Also the whole pacer when radio
/// notifications are unavailable (`idle_age_ms() == None`) — that fallback
/// is exactly the validated fixed-anchor regime.
const MIN_POLL_SPACING_MS: u64 = 13;

/// Hard cap on waiting for a quiet-window edge: a missing or late INACTIVE
/// notification can slow one iteration to this, never stall the loop.
const POLL_FALLBACK_MS: u64 = 20;

/// A radio-INACTIVE edge no older than this marks the head of a quiet
/// window — the only place a Maple transaction is allowed to start when
/// notifications are live. 2ms spent, ~4.5ms gc, ~1.7ms VMU DMA still end
/// ~4ms before the next connection event.
const QUIET_FRESH_MS: u32 = 2;

/// Re-check cadence while waiting for a quiet-window edge.
const ALIGN_POLL_US: u64 = 500;

/// Cap on the pacer's edge wait (the slice of `POLL_FALLBACK_MS` left after
/// the minimum spacing).
const POLL_ALIGN_CAP_MS: u64 = POLL_FALLBACK_MS - MIN_POLL_SPACING_MS;

/// Cap on a mid-iteration edge wait (VMU probe/enumerate). Slightly over
/// one connection interval, so a live notification source always delivers
/// an edge inside it.
const EXTRA_ALIGN_CAP_MS: u64 = 18;

/// Wait until the head of a radio-quiet window — a fresh INACTIVE edge — or
/// `cap_ms` from now, whichever comes first. `None` (no notification
/// source) returns immediately: fixed-cadence fallback. Returns the time
/// actually spent waiting so callers can keep it out of body-time budgets.
///
/// Every Maple transaction is supposed to start through this. The v216
/// soak showed why mid-iteration transactions need it too: a VMU-probe
/// iteration ran `get_condition` + `sub_peripheral_mask` + `enumerate_vmu`
/// back-to-back (~20ms of bus time against a ~12ms quiet window), so the
/// tail transactions collided every time and four collided probes in a row
/// (12s) flipped VMU presence — the "brief VMU disconnect" during the soak.
async fn align_to_quiet_window(cap_ms: u64) -> Duration {
    let start = Instant::now();
    let deadline = start + Duration::from_millis(cap_ms);
    loop {
        match pulsar_dreamcast_ble::maple::radio_notify::idle_age_ms() {
            Some(age) if age <= QUIET_FRESH_MS => break,
            None => break,
            Some(_) => {
                if Instant::now() >= deadline {
                    break;
                }
                Timer::after(Duration::from_micros(ALIGN_POLL_US)).await;
            }
        }
    }
    start.elapsed()
}

/// Body-time budget for the on-device overrun detector (`POLL_OVERRUNS`).
/// Measured on v216 (run #44): collision-free gc is 6.6-7.2ms (decode is
/// the wide part), and a VMU-animation poll adds a ~1.7ms DMA write — so
/// honest bodies peak ~9ms (the first 9ms budget counted exactly those VMU
/// polls, ~4/s). 11 clears the honest peak while still catching a single
/// collision retry (+5-7ms), which is what this detector exists to see.
const BODY_BUDGET_MS: u64 = 11;

/// Consecutive poll failures before declaring controller lost.
const CONTROLLER_LOST_THRESHOLD: u16 = 30;

/// a scheduled VMU block read may take one window in this many.
///
/// **A ceiling, not the rate.** The v258 sweep measured the decoder, not the
/// ratio, as the pacer: ~25 ms per block spans several windows' slack, so the
/// real cadence came out at one read per 7-10 windows for every N from 2 to 5.
/// What N does buy is a floor under controller freshness when a decode happens
/// to be quick, and 5 is the least costly of the ratios the sweep measured
/// (doubled-interval skew 8.7 % against 15.8 % at N = 2).
///
/// **Chosen 2026-09-22: 5** — the sweep put none of N = 2-5
/// inside the HID band, so this is the conservative end of the measured range,
/// and it is the value the write path was benched at, so later write
/// runs stay comparable. Writes pace on it too: a block is five slots plus its
/// read-back, ≈ 0.6 s, ≈ 12 s for a 20-block save. Revisit only with the HID
/// gate run under a drain beside the change.
#[cfg(feature = "spim-capture")]
const READ_EVERY_N: u8 = 5;

/// decode slices run in the iteration's slack until this far past
/// its start, and none is begun within [`SLICE_GUARD_US`] of it.
///
/// Continuity with the tested configuration, as `READ_LCD_GAP_TICKS` is: 14 ms
/// with 1,024-sample slices is what the v263-v299 pulls were measured under
/// (185 blocks in 18.2-18.5 s). It deliberately exceeds `MIN_POLL_SPACING_MS`,
/// so a window that is still decoding reaches the pacer with its spacing timer
/// already expired and starts the next window at the next quiet edge instead.
/// The slices run *after* the body-budget check, so a decode is never counted
/// as a poll overrun — it is slack work, not body work.
#[cfg(feature = "spim-capture")]
const DECODE_UNTIL_MS: u64 = 14;

/// A decode slice is not begun within this of [`DECODE_UNTIL_MS`].
#[cfg(feature = "spim-capture")]
const SLICE_GUARD_US: u64 = 500;

/// Initial retry delay for controller detection (ms).
const INITIAL_RETRY_DELAY_MS: u64 = 100;

/// Maximum retry delay for controller detection (ms).
const MAX_RETRY_DELAY_MS: u64 = 1000;

/// How often to check BLE connection state while waiting (ms).
const BLE_WAIT_CHECK_MS: u64 = 100;

/// How long the 5 V rail stays up after a link drops.
///
/// ADR-005 drops the rail at every disconnect, which on pulsarv1 power-cycles
/// the controller, VMU and rumble pack. At the edge of range a link drops and
/// comes back within ~0.05–3 s (bench, 2026-09-24), so each blip also cost
/// a controller reset and a re-detect. Ten seconds is one `ReconnectFast`
/// burst: if the unit's own fast reconnect hasn't landed by then, the host
/// isn't coming back soon and the rail goes down as ADR-005 intends.
const RAIL_GRACE: Duration = Duration::from_secs(10);

/// Why the 5 V rail is still up in Phase 1, where ADR-005 otherwise has it down.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RailHold {
    /// Down, as ADR-005 has it.
    Released,
    /// Sync mode with a controller on the port: the SYNC splash on the VMU is
    /// the pairing indicator and only stays lit while the rail does. Held
    /// until the sync window ends without a connect. Taken from Phase 3, where
    /// the controller is known, or from Phase 1 once the controller has
    /// answered during the splash — never for an empty port, where the no-bond
    /// auto-entry would run the boost into nothing for the whole ≤60 s window.
    Sync,
    /// A link just dropped: held until this instant so a reconnect inside
    /// [`RAIL_GRACE`] doesn't power-cycle the controller.
    Grace(Instant),
}

impl RailHold {
    /// A hold after a link drop, timed from the drop itself.
    ///
    /// From the drop, not from leaving the poll loop: when acked VMU saves are
    /// draining, the loop stays up to `LINK_LOSS_DRAIN_DEADLINE_MS` after the
    /// drop, and by then the `ReconnectFast` burst the grace is
    /// sized to has long ended. A grace that has already run out drops the
    /// rail on Phase 1's first pass.
    fn after_drop(dropped_at: Instant) -> Self {
        Self::Grace(dropped_at + RAIL_GRACE)
    }

    /// Whether Phase 1 should drop the rail now. A connect never gets here:
    /// Phase 2 takes the rail over and releases the hold.
    fn is_over(self, conn_state: ConnectionState, now: Instant) -> bool {
        match self {
            Self::Released => false,
            Self::Sync => conn_state != ConnectionState::SyncMode,
            Self::Grace(until) => now >= until,
        }
    }
}

/// Timeout for initial controller detection (ms).
/// Enter System Off if no controller found within 60 seconds of BLE connecting.
const DETECT_TIMEOUT_MS: u64 = 60_000;

/// Timeout before entering sleep when controller is idle (ms).
/// 10 minutes with no input change triggers System Off.
const INACTIVITY_TIMEOUT_MS: u64 = 600_000;

/// How long the poll loop keeps the rail up after a BLE drop to finish
/// draining acked saves onto the VMU, before it gives up on them.
///
/// The storage lease: a block the dongle has been told is staged must
/// reach the card, link or no link, or its `WRITTEN DISCARDED` must say it did
/// not. A full queue is eight blocks at five bus transactions each — a few
/// seconds at the measured cadence, retries included — so 30 s is headroom,
/// and it is well inside the 60 s after which the BLE task asks for System
/// Off. Past it the loop leaves as it would have at the drop: the rail goes
/// down, the generation ends, and whatever is still staged is discarded with
/// a WRITTEN the dongle collects at its next connection.
const LINK_LOSS_DRAIN_DEADLINE_MS: u64 = 30_000;

/// Minimum battery percentage to allow an OTA DFU reboot. A unit that dies
/// mid-transfer isn't bricked (the bootloader is never touched), but it
/// strands the user in DFU mode on a draining battery. Charging is always
/// allowed regardless of level — external power is present. 50 is also a
/// clean threshold for pulsarv1's IP5306 gauge, which only reports in 25%
/// steps.
const DFU_MIN_BATTERY_PCT: u8 = 50;

/// The DFU battery gate, shared by every site that can act on `DFU_PENDING`.
///
/// `Some(verdict)` means refuse, and says why; `None` means allow. Always a fresh read, never the
/// 60s-cadence sample — the charging bit in particular has to be current, and a
/// user who just plugged in expects the gesture to work. The decision itself is
/// `battery_policy::dfu_verdict`, where it is tested: a board with no gauge is
/// allowed through, but a gauge that could not say how full the battery is
/// **refuses** off the cable. It used to allow, and a partial read failure could
/// discard a level that said "too low".
///
/// **VBUS short-circuits the gate.** The risk this exists for is being stranded
/// in DFU mode on a draining battery (ADR-014), and external power removes it
/// outright — so plugged in is sufficient regardless of what the gauge says. That
/// is deliberately checked *before* the IP5306, because both of its answers can
/// refuse a perfectly safe update: `charging` goes false the instant the pack tops
/// off, and `percent` comes from `0x78`, the undocumented LED-driver state. On
/// 2026-08-17 those two combined to lock a unit out of OTA *while sitting on a
/// charger* — reading 25 %, refusing, with no way to reach the bootloader. It is
/// also the way out of an "unknown" refusal: plug in, repeat the gesture.
async fn dfu_battery_refusal(power: &mut board::Power) -> Option<battery_policy::DfuVerdict> {
    if pulsar_dreamcast_ble::usb_vbus_present() {
        return None;
    }
    let verdict = battery_policy::dfu_verdict(power.battery().await, false, DFU_MIN_BATTERY_PCT);
    (verdict != battery_policy::DfuVerdict::Allowed).then_some(verdict)
}

/// Is a goodbye due now? The 7 s hold waits while acked saves are draining
/// onto the VMU; the 15 s hold (`SHUTDOWN_FORCED`) does not. The poll loop's
/// goodbye applies the same rule inline, where it also needs `forced` apart.
/// For both detection loops, which used to read neither flag, so a hold while
/// no controller answered waited out the detect timeout instead.
fn goodbye_due() -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    pulsar_dreamcast_ble::GOODBYE_PENDING.load(Relaxed)
        && (pulsar_dreamcast_ble::SHUTDOWN_FORCED.load(Relaxed)
            || !pulsar_dreamcast_ble::ble::host_vmu::draining())
}

/// Best-effort VMU splash from Phase 1, where the 5 V rail is down (ADR-005)
/// and so is the VMU. Brings the rail up, lets the boost and the controller
/// settle, then retries **enumerate + write** as a pair until the LCD takes it,
/// the budget runs out, or `superseded` says to stop.
///
/// One helper for every Phase 1 splash (BYE at goodbye, BOOT at chord DFU,
/// SYNC for a window opened while disconnected) because the sequence is the
/// same and the trap is the same: under the old always-on rail the VMU was
/// warm, so a single enumerate followed by write retries was enough. Cold,
/// the controller and VMU have to boot first, and the VMU refuses
/// `BLOCK_WRITE` until it has answered `DEVICE_INFO` — so an enumerate that
/// fired too early leaves every later write refused, however many times it is
/// retried. Enumerating inside the loop is what makes the
/// retries mean anything.
///
/// Budget: ~70 ms settle + `attempts` × ~100 ms, paid in full only when the
/// VMU never acks. BYE and BOOT pass [`VMU_SPLASH_ATTEMPTS`] (~1.1 s): their
/// callers sleep or reboot next either way. SYNC passes the longer
/// [`COLD_SYNC_SPLASH_ATTEMPTS`], because its caller then waits out the window
/// with whatever the attempt left on the LCD. Never blocks on failure.
async fn phase1_vmu_splash(
    power: &mut board::Power,
    bus: &mut MapleBus,
    host: &MapleHost,
    framebuf: &[u8; 192],
    attempts: usize,
    superseded: fn() -> bool,
) -> SplashOutcome {
    power.rail_on();
    // Same settle as the Phase 2 entry, then the bus wake the old paths used.
    Timer::after(Duration::from_millis(50)).await;
    bus.set_output_mode();
    Timer::after(Duration::from_millis(20)).await;
    vmu_splash_until_acked(bus, host, framebuf, attempts, superseded).await
}

/// Splash attempts for a warm VMU, or for a caller that sleeps or reboots next
/// (~1 s at the retry spacing).
const VMU_SPLASH_ATTEMPTS: usize = 10;

/// Splash attempts for SYNC from a cold rail (~2.5 s). The VMU has been seen
/// still waking ~1.5 s after power arrives (`MapleHost::enumerate_vmu`), past
/// the warm budget, and a SYNC that misses is not tried again that window.
/// A starting value: confirm it on the bench.
const COLD_SYNC_SPLASH_ATTEMPTS: usize = 25;

/// What a splash attempt learned about the port.
#[derive(Clone, Copy)]
struct SplashOutcome {
    /// The VMU acked the LCD write. `false` does not prove the frame is
    /// missing: an ack corrupted on the way back looks the same.
    acked: bool,
    /// The controller answered a device-info request. This, not `acked`, is
    /// what says there is anything on the port to light.
    controller_seen: bool,
}

/// Whether the controller, asked just now, says slot 1 is empty.
///
/// Asked fresh rather than read from the poll loop's `vmu_present`: that is up
/// to one 5 s probe interval old, and a streak of *unanswered* probes clears
/// it just as an empty slot does — interference alone can mark a docked VMU
/// absent. Only a decoded reply without the slot bit counts; no reply is
/// unknown, and unknown tries the splash. One exchange, ≤2 ms when nothing
/// answers.
fn vmu_known_absent(host: &MapleHost, bus: &mut MapleBus) -> bool {
    host.sub_peripheral_mask(bus)
        .is_some_and(|m| m & pulsar_dreamcast_ble::maple::host::addressing::SUB_SLOT_1 == 0)
}

/// `superseded` for the BYE and BOOT splashes, which have nothing to yield to.
const fn never_superseded() -> bool {
    false
}

/// `superseded` for the SYNC splashes: stop retrying once something more
/// urgent than the indicator is waiting — a goodbye, a DFU request or a
/// sleep request, each handled by Phase 1 after the splash — or the sync
/// window has already ended.
fn sync_splash_superseded() -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    pulsar_dreamcast_ble::GOODBYE_PENDING.load(Relaxed)
        || pulsar_dreamcast_ble::DFU_PENDING.load(Relaxed)
        || pulsar_dreamcast_ble::SLEEP_REQUEST.load(Relaxed)
        || get_connection_state() != ConnectionState::SyncMode
}

/// The retry half of [`phase1_vmu_splash`], for callers whose rail is already
/// up: **enumerate + write** as a pair until the VMU acks the write, the
/// budget runs out, or `superseded` — asked before every attempt — says to
/// stop. The splashes go out on the bit-bang path, which SoftDevice
/// interrupts can corrupt, so a single unchecked attempt can leave the old
/// frame on the LCD.
async fn vmu_splash_until_acked(
    bus: &mut MapleBus,
    host: &MapleHost,
    framebuf: &[u8; 192],
    attempts: usize,
    superseded: fn() -> bool,
) -> SplashOutcome {
    const RETRY_MS: u64 = 100;
    let mut outcome = SplashOutcome {
        acked: false,
        controller_seen: false,
    };
    for _ in 0..attempts {
        if superseded() {
            break;
        }
        // Asked until the controller answers once; a cold one may take a few
        // attempts to boot.
        if !outcome.controller_seen {
            outcome.controller_seen = host.sub_peripheral_mask(bus).is_some();
        }
        // Enumerate for its side effect (see `MapleHost::enumerate_vmu`): a
        // freshly docked VMU was seen to ignore LCD writes until asked for
        // device info.
        let _ = host.enumerate_vmu(bus);
        if host.write_vmu_lcd(bus, framebuf) {
            // An ack came back through the controller, so it is there even
            // if its own device-info reply was lost.
            outcome.acked = true;
            outcome.controller_seen = true;
            break;
        }
        Timer::after(Duration::from_millis(RETRY_MS)).await;
    }
    outcome
}

/// Low battery cutoff voltage (mV). Enter System Off below this.
/// 3.2V gives ~5% margin above the 3.0V "empty" threshold.
///
/// The cutoff only applies when `bat.millivolts > 0`, i.e. the board actually
/// reports a voltage. Boards with a coarse gauge that has no millivolt readout
/// (pulsarv1's IP5306 reports `millivolts: 0`) would otherwise trip this on
/// every battery reading — `0 < 3200` is always true — and force System Off the
/// instant they run off battery. Those boards use the percent cutoff below.
const LOW_BATTERY_CUTOFF_MV: u32 = 3200;

/// Low battery cutoff for gauges that report **no voltage**, only a percentage
/// (pulsarv1's IP5306). Enter System Off at or below this level.
///
/// Deliberately `0`, not a comfortable 10-15 %: the IP5306 is a coarse 4-LED
/// gauge whose register decode is still ⚠ UNVERIFIED, and "no LEDs lit" is the
/// reading least dependent on how the other codes decode. It is not itself
/// verified: the datasheet blinks the bottom LED below ~3 %, and whether `0x78`
/// returns a settled bucket or that instantaneous state is unmeasured — one
/// reason the cutoff wants repeated readings over time, never one. Raise this
/// only once a characterization of `0x78` (logged on every read,
/// `board::pulsarv1::Power::battery`) pins the map down; cutting off at a
/// mis-decoded 25 % would throw away a quarter of the pack's runtime.
const LOW_BATTERY_CUTOFF_PCT: u8 = 0;

/// Normal battery cadence.
const BATTERY_READ_INTERVAL: Duration = Duration::from_secs(60);

/// Battery cadence inside the detect loops — the IP5306 refresh cadence. A
/// detect lasts at most `DETECT_TIMEOUT_MS`, so at 60 s the confirmation could
/// never complete there and the gauge would be one reading old for the whole
/// search.
///
/// How often the battery is read does not decide how soon a unit is switched
/// off: the confirmation spans below are per context, not a by-product of this
/// cadence. It stays detect-only for now because nothing yet says a working
/// unit needs fresher readings — a provisional call the instrumented discharge
/// run is meant to settle.
const BATTERY_FAST_INTERVAL: Duration = Duration::from_secs(10);

/// How long a detect runs unanswered before the gauge is lit beside the red
/// status LED. A docked controller answers within the first few retries; the
/// grace keeps an ordinary connect from flashing the bars at a VMU owner, which
/// is the 2026-07-27 flash the Phase 3 `presence_known` gate exists to prevent.
const DETECT_GAUGE_GRACE: Duration = Duration::from_secs(2);

/// The low-battery rule's numbers. The rule itself, and its tests, are in the
/// `battery-policy` crate — this firmware crate cannot run a test, and this is
/// the logic that decides when a unit powers itself off.
///
/// Starting values to bench, not characterized ones:
/// - three counted empty readings ([`LOW_BATTERY_CUTOFF_PCT`] or below), so one
///   glitched I²C read can't power down a healthy board — the failure class
///   that cost the 2026-07-24 debugging session — and a boot reading alone
///   never can;
/// - spanning 20 s while nothing answers on the bus, 120 s while a controller
///   does (what three readings at the 60 s cadence always came to);
/// - readings under 8 s apart count once, so a phase change — which takes a
///   reading of its own — cannot accelerate the cutoff;
/// - a gap over 150 s starts the evidence over (two and a half normal
///   intervals: one missed read is forgiven);
/// - a reading older than 150 s is no longer displayed as current. The same
///   number as the gap for now, but a separate knob: one is about evidence,
///   the other about presentation.
const BATTERY_POLICY: battery_policy::Config = battery_policy::Config {
    can_sleep: board::SUPPORTS_SLEEP,
    cutoff_millivolts: LOW_BATTERY_CUTOFF_MV,
    // Off: the xiao has never been sampled while searching, and a
    // single reading under the boost's start-up load must not sleep it. See
    // the field's own doc for what turning it on would need first.
    voltage_cutoff_while_searching: false,
    cutoff_percent: LOW_BATTERY_CUTOFF_PCT,
    empty_reads: 3,
    confirm_span_searching_ms: 20_000,
    confirm_span_normal_ms: 120_000,
    min_spacing_ms: 8_000,
    max_gap_ms: 150_000,
    display_expiry_ms: 150_000,
};

/// Observation → policy → action, for every phase.
///
/// This is the *action* third and the scheduling: it asks the board for a
/// [`board::Reading`], hands the observation to [`battery_policy::Policy`], and
/// carries out the [`battery_policy::Decision`]. It decides nothing itself.
/// Phases differ in how urgently they sample and in what they show; none of
/// them owns cutoff timing.
struct BatteryMonitor {
    policy: battery_policy::Policy,
    last_attempt: Instant,
}

impl BatteryMonitor {
    fn new() -> Self {
        Self {
            policy: battery_policy::Policy::new(BATTERY_POLICY),
            last_attempt: Instant::now(),
        }
    }

    /// Is a reading due?
    fn due(&self, context: battery_policy::Context) -> bool {
        let interval = match context {
            battery_policy::Context::Searching => BATTERY_FAST_INTERVAL,
            battery_policy::Context::Normal => BATTERY_READ_INTERVAL,
        };
        self.last_attempt.elapsed() >= interval
    }

    /// What is fit to show right now. Every display reads this — the VMU icon,
    /// the WS2812 gauge, in every phase — so a reading taken in one phase is not
    /// lost to the next, and a stale one is not shown as current.
    fn snapshot(&self) -> battery_policy::Snapshot {
        self.policy.snapshot(Instant::now().as_millis())
    }

    /// Read the battery, publish the level for BLE, and carry out the policy's
    /// decision. One path for every call site (boot, Phase 1 wait, Phase 3 poll,
    /// and the two detect loops via [`DetectBatteryWatch`]) because they must
    /// not drift. Returns the snapshot after the reading.
    ///
    /// # Safety
    /// May enter System Off and never return — see [`sleep_now`].
    async unsafe fn sample(
        &mut self,
        power: &mut board::Power,
        status: &mut board::StatusIndicator,
        context: battery_policy::Context,
    ) -> battery_policy::Snapshot {
        use battery_policy::Decision;

        self.last_attempt = Instant::now();
        let board::Reading::Observed(obs) = power.battery().await else {
            return self.snapshot();
        };
        // Charging reports as 0xFF; an unknown level reports nothing. The BLE
        // Battery Level the host holds is therefore *last known*, not current:
        // the characteristic has no way to say "unknown", and neither a
        // made-up 0 % nor the internal 0xFF sentinel may stand in for one. A
        // client of ours that needs validity or age wants a field of its own.
        //
        // Charge-complete on the cable is tested first: `0xFF` tells the BLE
        // task to leave the published level alone, so a chip that reported
        // complete *and* charging together would otherwise never publish the
        // 100. Which flag combinations the IP5306 really produces through
        // termination has not been captured on hardware. This is the same rule
        // as `Snapshot::shown_percent`, applied to the fresh observation.
        let complete_on_cable =
            obs.charge_complete == Some(true) && pulsar_dreamcast_ble::usb_vbus_present();
        match (complete_on_cable, obs.charging, obs.percent) {
            (true, _, _) => BATTERY_LEVEL.signal(100),
            (false, Some(true), _) => BATTERY_LEVEL.signal(0xFF),
            (false, _, Some(percent)) => BATTERY_LEVEL.signal(percent),
            (false, _, None) => {}
        }

        match self
            .policy
            .observe(obs, Instant::now().as_millis(), context)
        {
            Decision::Continue => {}
            Decision::Warn => {
                log!(
                    "PWR: Gauge empty ({:?}), counted readings/span ms: {:?}",
                    obs,
                    self.policy.empty_progress()
                );
            }
            Decision::Shutdown(_reason) => {
                log!("PWR: Low battery ({:?}), entering System Off", _reason);
                // SAFETY: `sleep_now` requires an initialised SoftDevice and never returns.
                // Both hold here: the SoftDevice is enabled during setup, well before this
                // point, and this call diverges — nothing after it can observe the
                // torn-down pin state.
                unsafe {
                    sleep_now(power, status);
                }
            }
        }
        self.snapshot()
    }
}

/// What a detect loop adds to [`BatteryMonitor`]: urgency and presentation.
/// Used by Phase 2 and by the Phase 3 re-detect after a controller is lost.
///
/// On pulsarv1 a low cell and a missing controller are the same symptom: the
/// IP5306's Batlow cutoff drops only the 5 V boost, while the MCU runs on from
/// LDO1 off `+BATT`, so the link stays up and nothing answers on the bus.
/// Nothing documented reports the boost output either — the register document's
/// reads are `0x70[3]` charging, `0x71[3]` full, `0x72[2]` light load and the
/// `0x77` key flags — and `SYS_CTL0` holds enables, not output state. So the
/// detect loops cannot *know* the cell is the reason; what they can do is show
/// the gauge, so red beside a red first bar reads as "charge me", and sample
/// fast enough that a flat cell reaches System Off instead of searching on it.
///
/// Whether a VMU is docked is unknowable while nothing answers, so the gauge is
/// shown regardless; Phase 3's presence logic hides it again once a VMU replies.
/// It is shown on silence alone — not on any evidence of *why* — because a
/// battery level beside red is harmless when the cause is an unplugged pad.
struct DetectBatteryWatch {
    started: Instant,
    /// Whether this watch has put anything on the gauge yet, and what.
    applied: bool,
    shown: Option<u8>,
}

impl DetectBatteryWatch {
    /// Take a reading on entry to a detect loop, before its first Maple
    /// exchange — battery reads are I²C and stay off the bus transaction. The
    /// reading is for the display; whether it *counts* toward the cutoff is the
    /// policy's call.
    ///
    /// # Safety
    /// May enter System Off and never return — see [`BatteryMonitor::sample`].
    async unsafe fn start(
        battery: &mut BatteryMonitor,
        power: &mut board::Power,
        status: &mut board::StatusIndicator,
    ) -> Self {
        // SAFETY: `BatteryMonitor::sample`'s contract is this function's own
        // `# Safety` contract, passed straight through to the caller.
        unsafe {
            battery
                .sample(power, status, battery_policy::Context::Searching)
                .await;
        }
        Self {
            started: Instant::now(),
            applied: false,
            shown: None,
        }
    }

    /// Once per pass of a detect loop, between Maple exchanges: re-sample when
    /// one is due, and keep the gauge showing the shared snapshot once the
    /// detect has outlasted `DETECT_GAUGE_GRACE`.
    ///
    /// # Safety
    /// May enter System Off and never return — see [`BatteryMonitor::sample`].
    async unsafe fn tick(
        &mut self,
        battery: &mut BatteryMonitor,
        power: &mut board::Power,
        status: &mut board::StatusIndicator,
    ) {
        if battery.due(battery_policy::Context::Searching) {
            // SAFETY: `BatteryMonitor::sample`'s contract is this function's own
            // `# Safety` contract, passed straight through to the caller.
            unsafe {
                battery
                    .sample(power, status, battery_policy::Context::Searching)
                    .await;
            }
        }
        if self.started.elapsed() < DETECT_GAUGE_GRACE {
            return;
        }
        // The display fallback, here as everywhere: a level that was never
        // read, or has gone stale, is hidden rather than left lit as current.
        // `set_battery` writes the strip only on a real change. The shown
        // level, not the measured one — this feeds the LED bar, never a
        // cutoff; VBUS is read here so a cable plugged in mid-search shows
        // its 100 without waiting for the next reading.
        let percent = battery
            .snapshot()
            .shown_percent(pulsar_dreamcast_ble::usb_vbus_present());
        if !self.applied || self.shown != percent {
            status.set_battery(percent);
            self.applied = true;
            self.shown = percent;
        }
    }
}

/// Re-assert the power IC's rail-up configuration and log what happened. A
/// failed read or a failed repair is said out loud: both leave the rail in an
/// unknown state, and neither is "no drift".
async fn refresh_power_config(power: &mut board::Power) {
    match power.refresh_config().await {
        board::ConfigRefresh::Unchanged => {}
        board::ConfigRefresh::Repaired => {
            log!("PWR: IP5306 config had drifted — boost/charger re-enabled");
        }
        board::ConfigRefresh::ReadFailed => {
            log!("PWR: IP5306 config read FAILED — rail state unverified");
        }
        board::ConfigRefresh::RepairFailed => {
            log!("PWR: IP5306 config had drifted and the repair write FAILED");
        }
    }
}

/// Blank the LEDs and power the board's 5 V rail down (so neither can drain the
/// battery in System Off), then enter deep sleep. Single choke point for every
/// sleep path; `prepare_for_sleep` is a no-op on boards with no switchable rail
/// (and on the XIAO, which powers its boost off inside `enter_sleep`).
///
/// The blanking is not cosmetic. On pulsarv1 `prepare_for_sleep` drops the 5 V
/// boost, but the WS2812 rail (`NEOPIXEL_3V3+`) is an ME6211 LDO fed straight off
/// `+BATT` with no enable line — nothing in software can switch it. A WS2812
/// holds its last frame for as long as it has power, so whatever the bar was
/// showing at sleep would stay lit off the battery until flat. This is new
/// exposure: the strip never worked before, so every prior sleep-current figure
/// was measured with an accidentally dark bar.
///
/// # Safety
/// Does not return; the `SoftDevice` must be initialized (see `board::enter_sleep`).
unsafe fn sleep_now(power: &mut board::Power, status: &mut board::StatusIndicator) -> ! {
    status.off();
    power.prepare_for_sleep();
    // SAFETY: `board::enter_sleep` requires an initialised SoftDevice and never
    // returns — both are this function's own `# Safety` contract (above), so the
    // obligation passes straight through to our caller. The two calls before it
    // are the ordering this function exists to enforce: the status bar is dark
    // and the boost is powered down *before* the chip stops executing, because
    // nothing can turn them off afterwards.
    unsafe { board::enter_sleep() }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    static GAMEPAD_SERVER: StaticCell<ble::GamepadServer> = StaticCell::new();
    static CONFIG_SERVER: StaticCell<ble::ConfigServer> = StaticCell::new();
    static BONDER: StaticCell<ble::Bonder> = StaticCell::new();

    log_init!();
    pulsar_dreamcast_ble::panic_handler::check_panic_log();
    log!("DC Adapter Starting");

    // Initialize Embassy with interrupt priorities that don't conflict with SoftDevice
    let mut config = embassy_nrf::config::Config::default();
    config.gpiote_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    config.time_interrupt_priority = embassy_nrf::interrupt::Priority::P2;

    // Owner access to the debug port is a deliberate product decision (ADR-015),
    // so state it rather than inheriting a library default that a future Embassy
    // upgrade could change underneath us. On build code F and later the nRF52840
    // locks the access port at reset unless firmware says otherwise: this makes
    // `init` write UICR.APPROTECT = HwDisabled and APPROTECT.DISABLE, resetting
    // once if the UICR word actually changed.
    //
    // This is a backstop, not the mechanism. Factory programming provisions the
    // same UICR word after the final chip erase, because a unit that never
    // reaches this line — bricked or unprogrammed — would otherwise be locked
    // with only a destructive `--recover` to open it, taking the panic log and
    // bonds with it.
    config.debug = embassy_nrf::config::Debug::Allowed;

    board::configure_embassy(&mut config);
    let p = embassy_nrf::init(config);

    // Embassy init may perform one APPROTECT-provisioning software reset, so
    // consume the retained one-boot marker only after it returns. This is still
    // before any SoftDevice enable, address selection, GATT registration, or
    // bond access.
    let config_mode = pulsar_dreamcast_ble::take_config_boot_marker();

    // Silicon housekeeping: clear bootloader pin residue, then park the onboard
    // QSPI flash in Deep Power Down (no-op on boards that need neither).
    // SAFETY: `board::early_init` must run before any Embassy pin peripheral is
    // configured. `embassy_nrf::init` above only hands back the `Peripherals`
    // struct; no pin has been claimed from `p` yet, so the contract holds.
    unsafe {
        board::early_init();
    }

    // Load durable prefs (active profile + remap) from the journal;
    // defaults to Xbox / RemapTable::DEFAULT on first boot.
    let prefs = ble::prefs::load_prefs();
    let profile_id = prefs.profile_id;
    let profile = profile_id.profile();
    log!(
        "PROFILE: {} (PID {:#06x})",
        core::str::from_utf8(profile.vmu_label).unwrap_or("?"),
        profile.pid
    );

    // Initialize exactly one SoftDevice personality. The GAP name is fixed at
    // enable time, so this decision must precede GATT server registration.
    ble::softdevice::set_profile(profile);
    let sd = if config_mode {
        log!("BOOT: isolated configuration personality");
        let sd = ble::softdevice::init_config_softdevice();
        ble::config::activate_config_address(sd);
        sd
    } else {
        ble::softdevice::init_softdevice(profile)
    };

    // Power-fail canary for the VMU-write SD-assert investigation: the VMU
    // draws its dock power from the shared 5V rail and its LCD/buzzer
    // activity may dip the supply during writes (debug log 2026-06-11). The
    // power-fail comparator fires a SOC event (logged in softdevice_task)
    // when VDD drops below 2.5V — any POFWARN correlated with a VMU write
    // is direct evidence for the rail-dip theory.
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "threshold-then-enable is one power-fail-comparator configuration; \
                  enabling before the threshold is set would arm it at the reset default"
    )]
    // SAFETY: both are SoftDevice SVC calls taking small integer arguments by
    // value and dereferencing nothing. The SoftDevice is enabled by this point,
    // which is their only precondition, and both return codes are discarded
    // deliberately — power-fail warning is diagnostic, so a refusal is not
    // fatal to boot.
    unsafe {
        use nrf_softdevice_s140 as sd_raw;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "SoftDevice power-threshold constants are small enum discriminants that fit u8"
        )]
        let _ = sd_raw::sd_power_pof_threshold_set(
            sd_raw::NRF_POWER_THRESHOLDS_NRF_POWER_THRESHOLD_V25 as u8,
        );
        let _ = sd_raw::sd_power_pof_enable(1);
    }

    // Radio notifications: RE-ENABLED 2026-08-05 to gate the poll loop's
    // Maple transactions into radio-quiet windows (see POLL_PERIOD_MS docs —
    // field data proved collisions, not codegen, drive the layout lottery).
    //
    // History: the 2026-06-10 "every gate built on these asserted" verdict
    // (see maple/radio_notify.rs) has a known confound discovered a day
    // later: those diagnostic builds carried poll_timing's critical-section
    // bug, whose SD asserts were triggered by the VMU-write measurement path
    // — which only ran when writes were active, exactly matching the
    // "writes-on asserts, writes-off clean" evidence. The notification
    // config itself (INT_ON_BOTH) ran hours clean elsewhere. Not proven
    // innocent: the re-enable is gated on a soak test (historical assert
    // rate was ~1-2/min, so a 30-60 min clean soak is decisive).
    if pulsar_dreamcast_ble::maple::radio_notify::init() {
        log!("RADIO: notification gate enabled (INT_ON_BOTH, 800us)");
    } else {
        log!("RADIO: notification cfg REJECTED — poll pacer in fixed-cadence fallback");
    }

    // Register exactly one runtime GATT database. The config branch never
    // constructs a Bonder, takes a flash handle, restores system attributes,
    // or calls the normal HID connection handler.
    if config_mode {
        let Ok(server) = ble::ConfigServer::new(sd) else {
            loop {
                cortex_m::asm::wfi();
            }
        };
        let server = CONFIG_SERVER.init(server);
        let _ = server.init(&prefs);

        if let Ok(token) = softdevice_task(sd) {
            spawner.spawn(token);
        }
        if let Ok(token) = ble::config::config_task(sd, server, prefs) {
            spawner.spawn(token);
        }
    } else {
        let Ok(server) = ble::GamepadServer::new(sd) else {
            loop {
                cortex_m::asm::wfi();
            }
        };
        let server = GAMEPAD_SERVER.init(server);
        let _ = server.init(profile);

        if let Ok(token) = softdevice_task(sd) {
            spawner.spawn(token);
        }

        let bonder = BONDER.init(ble::Bonder::new());
        if let Some((master_id, enc_info, peer_id, sys_attrs)) = ble::flash_bond::load_bond() {
            bonder.load_from_flash(master_id, enc_info, peer_id, sys_attrs);
        }
        if let Ok(token) = ble::task::ble_task(sd, server, bonder, prefs.remap) {
            spawner.spawn(token);
        }
    }

    // Initialize board-specific pins and peripherals (the board grabs whatever
    // pins/peripherals it needs from `p`; main never names an individual pin).
    let board::BoardPins {
        sdcka,
        sdckb,
        sync_button,
        sync_led,
        mut status,
        mut power,
        mut rumble,
        // The RX capture backend on the boards that have one (
        // route (a)); the parts are not even taken without the feature (see
        // `board`), because taking and dropping them for no consumer rolled
        // the poll loop (v278).
        #[cfg(feature = "spim-capture")]
        spim_capture,
    } = board::init(p);

    if !config_mode {
        if let Ok(token) = pulsar_dreamcast_ble::button::sync_button_task(sync_button, sync_led) {
            spawner.spawn(token);
        }
    }

    // Set up Maple Bus using Flex pins. Ahead of the first `.await` on
    // purpose: the capture's parts must be consumed before any suspension
    // point, or they land in the main task's future and grow its RAM symbol
    // (v267).
    let mut bus = MapleBus::new(sdcka, sdckb);
    // Open the VMU storage queue to the dongle wherever the writer
    // (`maple::block_write`) is compiled, which is wherever the capture is.
    // The DK has neither and keeps the queue closed (`queue_free` 0, every
    // WRITE answered FULL): a queue nothing drains is a queue of saves
    // `DISCARDED` at the next generation change. What makes an open queue
    // safe is the swap guard — the fingerprint compare in `write_seq`, and
    // this loop holding its re-enumeration while the queue drains (the probe
    // site below) — because presence takes four missed probes at 5 s to end a
    // generation, and a card swapped inside that window is caught by nothing
    // else. Opened in release builds 2026-09-22, and the guard's hardware
    // gate passed on 2026-09-25 (v326); it was a `vmu-write-bench` feature
    // before that.
    #[cfg(feature = "spim-capture")]
    pulsar_dreamcast_ble::ble::host_vmu::open_writes();
    // The bus samples every reply on the hardware capture from here on.
    // Built and handed over in one statement, ahead of the
    // next `.await`: a binding still live at a suspension point lands in the
    // main task's future and grows its RAM symbol, which moves the layout
    // (v267). `MapleBus::new` has enabled DWT `CYCCNT`, which the capture's
    // waits time by.
    #[cfg(feature = "spim-capture")]
    if let Some(parts) = spim_capture {
        // The board contract puts both Maple lines on P0, so the bit index in
        // `P0.IN` is the pin number `SpimCapture::new` selects MISO with.
        bus.attach_capture(SpimCapture::new(
            parts,
            u8::try_from(board::PIN_A_BIT).unwrap_or(0),
            u8::try_from(board::PIN_B_BIT).unwrap_or(0),
        ));
    }

    status.startup().await;

    // Log initial charge status
    let mut was_charging = {
        let charging = power.is_charging();
        log!(
            "PWR: {}",
            if charging { "Charging" } else { "Not charging" }
        );
        charging
    };

    let host = MapleHost::new();

    // Low-battery state for the whole run. Lives out here so the confirmation
    // survives the outer connect/disconnect loop.
    let mut battery = BatteryMonitor::new();

    // Initial battery read at startup
    // SAFETY: `BatteryMonitor::sample` is unsafe only because it may enter System
    // Off via `sleep_now` on a critically low reading, which carries that
    // function's contract: an initialised SoftDevice (enabled during setup,
    // before this point) and divergence if it fires.
    unsafe {
        battery
            .sample(&mut power, &mut status, battery_policy::Context::Normal)
            .await;
    }

    // The rail is normally down throughout Phase 1 (ADR-005). Two cases leave
    // it up and say why here (see `RailHold`): a sync window with a controller
    // on the port, and any link drop, for `RAIL_GRACE`. Phase 1 drops it once
    // the hold is over; a connect goes through Phase 2's `rail_on` (idempotent) and a
    // sleep through `prepare_for_sleep`, which drops it whatever the hold says.
    let mut rail_hold = RailHold::Released;
    // The current sync window has had its SYNC splash attempt — by the Phase 3
    // exit or by Phase 1 — whether or not it landed. Kept apart from
    // `rail_hold`: Phase 1 releases the rail when no controller answers, and
    // that must not read as "not yet tried" and retry every pass. Cleared once
    // the window ends.
    let mut sync_splash_tried = false;

    // Outer loop: wait for BLE connection, then poll controller
    loop {
        // --- Phase 1: Wait for BLE connection ---
        log!("MAIN: Waiting for BLE connection...");
        bus.set_low_power();
        status.off();
        loop {
            let conn_state = get_connection_state();
            if conn_state == ConnectionState::Connected {
                // A connect ends the window. Cleared here, not left to the
                // check below: a detection that fails back into sync mode
                // returns to Phase 1 without passing through Phase 3, and a
                // stale flag would skip its splash while the grace hold runs
                // out under the pairing window.
                sync_splash_tried = false;
                break;
            }
            if rail_hold.is_over(conn_state, Instant::now()) {
                log!("MAIN: rail hold over, rail off");
                power.rail_off();
                rail_hold = RailHold::Released;
            }

            if conn_state != ConnectionState::SyncMode {
                sync_splash_tried = false;
            }

            // Goodbye splash from disconnected state. Bring the rail up, try to
            // write BYE to the VMU (may silently fail if no controller is
            // plugged in), hold briefly so the user sees it, then sleep. Runs
            // on both boards so the flow is testable on the dev kit; XIAO
            // actually enters System Off, DK halts via WFI.
            //
            // The rail is down throughout Phase 1 (ADR-005), so the VMU is
            // unpowered until `phase1_vmu_splash` brings it up — without that
            // this splash could only ever render on a carrier whose rail
            // happened to be live. `sleep_now` brings the rail back down via
            // `prepare_for_sleep`.
            if pulsar_dreamcast_ble::GOODBYE_PENDING.load(core::sync::atomic::Ordering::Relaxed) {
                log!("MAIN: Phase 1 goodbye");
                {
                    let mut send_buf = pulsar_dreamcast_ble::vmu::build_message_splash(b"BYE");
                    pulsar_dreamcast_ble::vmu::rotate_180(&mut send_buf);
                    let outcome = phase1_vmu_splash(
                        &mut power,
                        &mut bus,
                        &host,
                        &send_buf,
                        VMU_SPLASH_ATTEMPTS,
                        never_superseded,
                    )
                    .await;
                    if outcome.acked {
                        log!("MAIN: BYE write OK");
                    } else {
                        log!("MAIN: BYE write failed (no controller?)");
                    }
                }
                Timer::after(Duration::from_millis(1000)).await;
                log!("MAIN: goodbye — entering sleep");
                // SAFETY: `sleep_now` requires an initialised SoftDevice and never returns.
                // Both hold here: the SoftDevice is enabled during setup, well before this
                // point, and this call diverges — nothing after it can observe the
                // torn-down pin state.
                unsafe {
                    sleep_now(&mut power, &mut status);
                }
            }

            // DFU request from the disconnected state. The Phase 3 site is
            // unreachable without a controller, and the tap-tap-hold chord that
            // arms DFU without one exists precisely for that case — so the flag
            // has to be consumed here too, exactly as GOODBYE_PENDING is above.
            // Without this the chord would set a flag nobody reads, and the reset
            // on the next sleep would silently discard it.
            if pulsar_dreamcast_ble::DFU_PENDING.swap(false, core::sync::atomic::Ordering::Relaxed)
            {
                if let Some(_verdict) = dfu_battery_refusal(&mut power).await {
                    // No CHRG splash here: that mechanism rides the Phase 3 VMU
                    // write path, and in Phase 1 there may be no VMU powered at
                    // all. The gesture can simply be retried on a charger.
                    log!(
                        "MAIN: Phase 1 DFU refused: {:?} (need {}% or charger)",
                        _verdict,
                        DFU_MIN_BATTERY_PCT
                    );
                } else {
                    log!("MAIN: Phase 1 DFU pending — BOOT splash, then OTA bootloader");
                    // Best-effort splash. There may be no controller and no VMU
                    // here — that is the whole point of the chord — so this is
                    // fire-and-forget and must not block the reboot beyond the
                    // helper's ~1 s budget. The rail is down in Phase 1, so the
                    // helper brings it up first; it then stays up through the
                    // bootloader, which is what keeps BOOT on the LCD while the
                    // update runs, and the app's own init brings it back down.
                    let mut send_buf = pulsar_dreamcast_ble::vmu::build_boot_splash(
                        pulsar_dreamcast_ble::installed_app_version(),
                    );
                    pulsar_dreamcast_ble::vmu::rotate_180(&mut send_buf);
                    let _ = phase1_vmu_splash(
                        &mut power,
                        &mut bus,
                        &host,
                        &send_buf,
                        VMU_SPLASH_ATTEMPTS,
                        never_superseded,
                    )
                    .await;
                    pulsar_dreamcast_ble::reboot_into_ota_dfu();
                }
            }

            // The BLE task's disconnected-state timeouts (reconnect timeout,
            // sync timeout with no bond) hand their sleep here instead of
            // calling enter_sleep() themselves, so the 5 V boost goes down with
            // us. Only checked in this loop: both requesting paths are
            // disconnected-only, so main is provably right here when the flag
            // is set, and `request_sleep` parks the BLE task until we act.
            if pulsar_dreamcast_ble::SLEEP_REQUEST.load(core::sync::atomic::Ordering::Relaxed) {
                log!("MAIN: BLE requested System Off");
                // SAFETY: `sleep_now` requires an initialised SoftDevice and never returns.
                // Both hold here: the SoftDevice is enabled during setup, well before this
                // point, and this call diverges — nothing after it can observe the
                // torn-down pin state.
                unsafe {
                    sleep_now(&mut power, &mut status);
                }
            }

            // SYNC splash for a sync window the Phase 3 exit did not see: a sync
            // press while disconnected, or the no-bond auto-entry at boot. Seen
            // blank after a profile switch, whose reset leaves the host bonded to
            // the old identity and the unit waiting here disconnected.
            //
            // After the goodbye, DFU and sleep checks above, so none of them
            // waits behind it, and `sync_splash_superseded` stops the retries if
            // one arrives mid-attempt. Not in config mode: `config_task` reports
            // `SyncMode` while it advertises the configuration personality, which
            // is not a pairing window.
            //
            // One attempt per window, landed or not. The rail stays up for the
            // window only if the controller answered; an unacked write with a
            // controller present may still have drawn (a lost ack looks the
            // same), and an empty port has nothing to light.
            if !config_mode && conn_state == ConnectionState::SyncMode && !sync_splash_tried {
                sync_splash_tried = true;
                log!("MAIN: Phase 1 sync mode, writing SYNC splash");
                let mut send_buf = pulsar_dreamcast_ble::vmu::build_message_splash(b"SYNC");
                pulsar_dreamcast_ble::vmu::rotate_180(&mut send_buf);
                let outcome = phase1_vmu_splash(
                    &mut power,
                    &mut bus,
                    &host,
                    &send_buf,
                    COLD_SYNC_SPLASH_ATTEMPTS,
                    sync_splash_superseded,
                )
                .await;
                bus.set_low_power();
                if outcome.controller_seen {
                    if !outcome.acked {
                        log!("MAIN: SYNC write not acked (may still have drawn)");
                    }
                    rail_hold = RailHold::Sync;
                } else if rail_hold == RailHold::Released {
                    log!("MAIN: no controller answered, rail off");
                    power.rail_off();
                }
                // Otherwise a grace hold owns the rail and ends it on its own
                // schedule.
            }

            {
                // Battery/charge monitoring while waiting for BLE
                let charging = power.is_charging();
                if charging != was_charging {
                    log!(
                        "CHG: {}",
                        if charging {
                            "Charging started"
                        } else {
                            "Charging stopped"
                        }
                    );
                    was_charging = charging;
                }

                if battery.due(battery_policy::Context::Normal) {
                    // SAFETY: `BatteryMonitor::sample` is unsafe only because it may enter
                    // System Off via `sleep_now` on a critically low reading, which carries
                    // that function's contract: an initialised SoftDevice (enabled during
                    // setup, before this point) and divergence if it fires.
                    unsafe {
                        battery
                            .sample(&mut power, &mut status, battery_policy::Context::Normal)
                            .await;
                    }
                }
            }

            Timer::after(Duration::from_millis(BLE_WAIT_CHECK_MS)).await;
        }
        log!("MAIN: BLE connected, enabling controller");
        // Drop anything the previous session left undrawn, before this one can
        // read it. `INGRESS` is a static and outlives the connection that
        // filled it, so a frame written but never drained would be handed to
        // this host as its first frame — and screen ownership is held until
        // the link drops, so it would hold the screen for the whole session.
        //
        // Here, at the *entry* to a session, rather than on the way out: the
        // poll loop's disconnect is only one of the exits. Phase 2 leaves
        // through `controller_found == false` when the link drops during
        // detection, and that path reaches none of Phase 3's teardown.
        pulsar_dreamcast_ble::ble::host_lcd::reset();
        // `host_vmu::reset` is *not* here: the VMU storage service's egress can
        // be notified by the BLE task before this loop reaches Phase 2, so it is
        // reset at the connection instead, in `ble::task::run_connection`.
        // Whatever the rail was held up for, Phase 2 owns it from here.
        rail_hold = RailHold::Released;

        // --- Phase 2: Enable the controller rail and detect the controller ---
        // Only carriers with a Schottky USB-5V passthrough (xiao) can skip their
        // boost while plugged in. pulsarv1 has no such path — its 5 V comes from
        // the IP5306 boost, switched here over I²C, and `is_externally_powered()`
        // there means "charging or topped off", which has nothing to do with who
        // feeds the rail. Gating on the capability keeps the log honest: on
        // pulsarv1 the rail is BLE-gated, never USB-gated.
        let mut usb_powered = board::HAS_USB_PASSTHROUGH && power.is_externally_powered();
        if usb_powered {
            log!("PWR: USB detected, boost off (passthrough)");
        } else {
            power.rail_on();
        }
        // Brief delay for power source startup
        Timer::after(Duration::from_millis(50)).await;

        // Re-assert the IP5306 here, right behind `rail_on()`, and again inside
        // the detect loop below. `rail_on` is a single best-effort I²C write; a
        // unit powered up before its cell was connected may not have answered
        // (historically that was `blocking_init`'s one ~10ms retry burst at boot,
        // and the boost never came on) — and the Phase 3 refresh site cannot
        // rescue it, because reaching Phase 3 requires the controller that the
        // dead rail is starving. Observed 2026-08-14: port at 3.9V (raw cell, not
        // boosted), nothing on the bus, board healthy over BLE the whole time. A
        // BLE link implies the cell is in, so this is the first moment the write
        // can actually land. `refresh_config` asserts the rail-up config, so it
        // belongs only here and in Phase 3 — never in Phase 1, where it would
        // undo `rail_off`.
        #[expect(
            clippy::items_after_statements,
            reason = "the constant is declared beside the loops that consume it; hoisting it to module scope would separate a tuning value from the only code it tunes"
        )]
        const IP5306_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
        refresh_power_config(&mut power).await;
        // `Instant::now()` only — never subtract a Duration from it, that panics
        // when the clock is younger than the value.
        let mut last_ip5306_refresh = Instant::now();

        status.searching();
        // Phase 2 had no battery read at all, so a cell too low to hold the boost
        // up was sixty seconds of red and a silent sleep — see `DetectBatteryWatch`.
        // SAFETY: `DetectBatteryWatch::start` is unsafe only because it may enter
        // System Off via `sleep_now` on a critically low reading, which carries that
        // function's contract: an initialised SoftDevice (enabled during setup,
        // before this point) and divergence if it fires.
        let mut detect_battery =
            unsafe { DetectBatteryWatch::start(&mut battery, &mut power, &mut status).await };
        let mut retry_delay_ms: u64 = INITIAL_RETRY_DELAY_MS;
        let mut timeout_logged = false;
        let detect_start = Instant::now();
        let controller_found = loop {
            // Abort detection if BLE disconnects
            if get_connection_state() != ConnectionState::Connected {
                break false;
            }

            // Enter System Off if no controller found within timeout
            if board::SUPPORTS_SLEEP && detect_start.elapsed().as_millis() >= DETECT_TIMEOUT_MS {
                log!(
                    "MAPLE: Detect timeout ({}s), entering System Off",
                    DETECT_TIMEOUT_MS / 1000
                );
                // SAFETY: `sleep_now` requires an initialised SoftDevice and never returns.
                // Both hold here: the SoftDevice is enabled during setup, well before this
                // point, and this call diverges — nothing after it can observe the
                // torn-down pin state.
                unsafe {
                    sleep_now(&mut power, &mut status);
                }
            }

            // The sleep hold. No BYE splash: nothing on the bus is answering.
            if goodbye_due() {
                log!("MAIN: Phase 2 goodbye — entering sleep");
                // SAFETY: as at the detect timeout above — an initialised
                // SoftDevice, and this call diverges.
                unsafe {
                    sleep_now(&mut power, &mut status);
                }
            }

            // DFU request during detection — the single likeliest moment to want
            // one. A host is connected but no controller is answering, either
            // because none is docked or because the Maple side has stopped
            // working; both leave Phase 3 unreachable. Without this the flag would
            // sit unread until DETECT_TIMEOUT_MS put the unit into System Off, and
            // the reset would clear it.
            if pulsar_dreamcast_ble::DFU_PENDING.swap(false, core::sync::atomic::Ordering::Relaxed)
            {
                if let Some(_verdict) = dfu_battery_refusal(&mut power).await {
                    log!(
                        "MAIN: Phase 2 DFU refused: {:?} (need {}% or charger)",
                        _verdict,
                        DFU_MIN_BATTERY_PCT
                    );
                } else {
                    log!("MAIN: Phase 2 DFU pending — OTA bootloader");
                    // No splash attempt: by definition nothing on the bus is
                    // answering here, so a write would only add latency before
                    // the reboot.
                    pulsar_dreamcast_ble::reboot_into_ota_dfu();
                }
            }

            // Keep re-asserting through detection, too. Nothing draws on the 5V
            // rail until the controller answers, and the IP5306's light-load dwell
            // is as short as 8s (`SYS_CTL2[3:2]`) — so the boost can drop out
            // *during* a detect that runs up to DETECT_TIMEOUT_MS.
            if last_ip5306_refresh.elapsed() >= IP5306_REFRESH_INTERVAL {
                refresh_power_config(&mut power).await;
                last_ip5306_refresh = Instant::now();
            }

            // SAFETY: as at `DetectBatteryWatch::start` above — may enter System
            // Off via `sleep_now`, whose contract (an initialised SoftDevice,
            // divergence if it fires) holds here.
            unsafe {
                detect_battery
                    .tick(&mut battery, &mut power, &mut status)
                    .await;
            }

            status.tx_activity_on();
            let result = host.request_device_info(&mut bus);
            status.tx_activity_off();

            match &result {
                MapleResult::Ok(_) => {
                    status.connected();
                    log!("MAPLE: Controller detected");
                    break true;
                }
                MapleResult::Timeout => {
                    if !timeout_logged {
                        log!("MAPLE: Timeout (retrying...)");
                        bus.diagnose_bus();
                        timeout_logged = true;
                    }
                }
                MapleResult::UnexpectedResponse(_cmd) => {
                    log!("MAPLE: Unexpected cmd=0x{:02X}", _cmd);
                }
            }

            Timer::after(Duration::from_millis(retry_delay_ms)).await;
            retry_delay_ms = (retry_delay_ms * 2).min(MAX_RETRY_DELAY_MS);
        };

        if !controller_found {
            log!("MAIN: BLE disconnected during controller detection");
            // Held, not dropped: a reconnect inside the grace finds the
            // controller already powered. A rumble command the
            // host sent during detection goes with it — Phase 3 applies
            // whatever is latched, and the next session never asked for it.
            pulsar_dreamcast_ble::RUMBLE_LEVEL.reset();
            rail_hold = RailHold::after_drop(Instant::now());
            continue;
        }

        // --- Phase 3: Poll loop (active gaming) ---
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a compile-time constant division (3_000 / 15 = 200) that plainly fits u16"
        )]
        let mut vmu_delay: u16 = (3_000 / POLL_PERIOD_MS) as u16; // ~3s before VMU attempt

        // `IP5306_REFRESH_INTERVAL` and `last_ip5306_refresh` are declared above
        // Phase 2 now, so the detect loop can re-assert the boost as well — see
        // the comment there for why that placement is load-bearing.

        // Is a VMU actually docked? `enumerate_vmu` is a real probe — it sends
        // DEVICE_INFO_REQUEST to sub-peripheral 1 and returns true only on a
        // valid response — so this both detects dock/undock and re-enumerates a
        // VMU that power-cycled. It replaces the old `vmu_enumerated` latch,
        // which only reset on controller-loss and so missed the common case
        // where the VMU resets but the controller never misses a poll.
        //
        // Slow cadence, not per-poll: a failed probe costs the full
        // `timeout_us` (2ms wall-clock) plus TX. Gating LCD writes on
        // presence more than pays for it — without this we fired a ~1.7ms DMA
        // write into the void every 20 polls whenever no VMU was docked.
        // 5s, not 3: every probe pass costs 1-2 extra quiet windows (the
        // pass spans multiple windows since the per-transaction alignment
        // fix), and each skipped window is a conn event with no fresh
        // input — run #45 measured the 3s cadence at ~1-1.5% of the
        // doubled-interval budget. Dock detection ≤5s, absence in 20s;
        // both fine for a display.
        #[expect(
            clippy::items_after_statements,
            reason = "the constant is declared beside the loop that consumes it; hoisting it to module scope would separate a tuning value from the only code it tunes"
        )]
        const VMU_PROBE_INTERVAL: Duration = Duration::from_secs(5);
        // Consecutive failed probes before believing the VMU is really gone. A
        // probe is a request/response transaction that must survive BLE
        // collisions (~64% of Maple frames collide with a connection event and
        // are dropped), so one failure means nothing. Presence is sticky.
        #[expect(
            clippy::items_after_statements,
            reason = "the constant is declared beside the loop that consumes it; hoisting it to module scope would separate a tuning value from the only code it tunes"
        )]
        const VMU_ABSENT_STREAK: u8 = 4;
        let mut vmu_present = false;
        let mut vmu_probe_misses: u8 = 0;
        // Probe passes since the last VMU re-enumerate (see the probe site:
        // re-arm on dock transitions and every 3rd pass, not every pass).
        let mut vmu_enum_passes: u8 = 0;
        // Has any probe actually *answered* yet this session? `vmu_present`
        // starts `false`, which is indistinguishable from "no VMU docked" — so
        // rendering the gauge before the first decodable reply flashes the bars
        // on a board that does have a VMU. Observed 2026-07-27, twice, right
        // after reflashing; hard to reproduce because it needs the 60 s battery
        // read to fall inside the short window before the first reply lands.
        let mut presence_known = false;
        // `None` = probe on the next pass. Do NOT express "probe immediately" as
        // `Instant::now() - VMU_PROBE_INTERVAL`: embassy's clock starts at zero
        // and `Sub<Duration>` is `checked_sub().expect(..)`, so that panics when
        // Phase 3 is reached less than VMU_PROBE_INTERVAL after boot — which is
        // the *normal* case when reconnecting to a bonded host. That bricked a
        // module on 2026-07-25.
        let mut last_vmu_probe: Option<Instant> = None;
        let mut vmu_frame_dirty = true;
        // the VMU storage read pipeline and the schedule that paces
        // it. Both are per-connection on purpose — a read is owed to a client
        // that is gone the moment the link drops, so nothing about one survives
        // the session (the write queue, which does, is a later step and keeps
        // its own lifetime). `lcd_end` is the schedule's gap clock: the return
        // of the last LCD frame, which is what a read has to stand clear of.
        #[cfg(feature = "spim-capture")]
        let mut reader = pulsar_dreamcast_ble::maple::block_read::BlockReader::new();
        // The writer, per session like the reader — but the block it holds
        // is the service's, and the service is what says whether it is still
        // the writer's to finish (`host_vmu::writing`, asked at every window
        // head). A block dropped here with the loop is a block the service
        // has already marked `DISCARDED`, because leaving the loop ends the
        // generation.
        #[cfg(feature = "spim-capture")]
        let mut writer = pulsar_dreamcast_ble::maple::block_write::BlockWriter::new();
        #[cfg(feature = "spim-capture")]
        let mut read_sched = maple_protocol::read_sched::ReadSched::new();
        #[cfg(feature = "spim-capture")]
        let mut lcd_end: Option<Instant> = None;
        // is the pad being used? Protocol v1 serves a READ only
        // after a second without input, so this is the gate on every host read
        // — per-connection like the reader, because a pad is "in use" until
        // this session's own samples say otherwise.
        #[cfg(feature = "spim-capture")]
        let mut idle_watch = maple_protocol::input_idle::IdleWatch::new();
        let mut vmu_framebuf =
            pulsar_dreamcast_ble::vmu::build_profile_splash(profile.vmu_glyph, profile.vmu_label);
        let mut vmu_anim_step: u8 = 0;
        let mut vmu_anim_counter: u16 = 0;
        // Splash holds ~30s before transitioning to the pulsar. Derived from
        // the cadence period: with the anchored loop, polls-to-time is finally
        // an honest conversion instead of the old "~17ms per poll" estimate
        // (a bad layout roll used to stretch every poll-counted duration —
        // the lingering boot splash was a visible bad-roll symptom).
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a compile-time constant division (30_000 / 15 = 2_000) that plainly fits u16"
        )]
        let mut vmu_splash_polls: u16 = (30_000 / POLL_PERIOD_MS) as u16;
        // Polls to hold the Guide-chord "home" glyph (~1s)
        // before resuming normal content. 0 = not showing it.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::items_after_statements,
            reason = "a compile-time constant division (1_000 / 15 = 66) that plainly \
                      fits u16, declared beside the loop that consumes it"
        )]
        const VMU_HOME_POLLS: u16 = (1_000 / POLL_PERIOD_MS) as u16;
        let mut vmu_home_polls: u16 = 0;
        // Advance the animation every 20 polls (~260ms, ~4fps). Each frame is
        // a ~1.7ms hardware-timed DMA TX the CPU awaits through — ~0.5ms of
        // average poll period, no bus corruption possible.
        #[expect(
            clippy::items_after_statements,
            reason = "the constant is declared beside the loop that consumes it; hoisting it to module scope would separate a tuning value from the only code it tunes"
        )]
        const VMU_ANIM_INTERVAL: u16 = 20;
        // Host LCD frames : a console-side dongle pushes the game's
        // VMU screen over the vendor GATT service, and it replaces the local
        // animation for as long as frames keep arriving.
        //
        // Paced, not drawn on every poll, for two reasons that both live
        // outside this file. Each frame is a ~1.7 ms hardware-timed DMA TX, so
        // a frame per poll would spend ~11% of the poll period on the bus
        // where the animation spends ~0.6%; and a 192-byte GATT write
        // fragments into ~8 link-layer packets, lengthening the connection
        // events that the Maple poll has to fit *between* (the quiet-window
        // pacer at the top of this file).
        //
        // This is the gap enforced between frames, so one is drawn every
        // seventh poll: ~105 ms, ~9.5 fps, ~1.6% of the poll period, and frame
        // traffic in roughly one connection event in seven. That is the ~10 fps
        // the bench gate asks for. It is a tuning value and not part of
        // any contract — shorten it only with an `hid_capture.py` run beside
        // it.
        #[expect(
            clippy::items_after_statements,
            reason = "the constant is declared beside the loop that consumes it; hoisting it to module scope would separate a tuning value from the only code it tunes"
        )]
        const VMU_HOST_INTERVAL: u16 = 6;
        // Polls until the next host frame may be drawn (the pacer above).
        let mut vmu_host_wait: u16 = 0;
        // True while the host owns the screen. Set by the first host frame and
        // cleared only by a disconnect.
        //
        // This was a 10 s hold-off, and the bench (2026-09-10) said
        // what the old comment here said it would take to change it: the
        // animation returning under a game's art reads as a fault. A real VMU
        // holds the last image written to it until something overwrites it,
        // and a host frame is treated the same way. A timer also cannot be
        // right at any value — a game parked at a menu draws nothing for
        // minutes and is still the thing on screen.
        //
        // Released at the drop, in the disconnect branch below. This used to
        // rely on the poll loop `break`ing out on every disconnect and the
        // declaration being re-run; now a drop with saves still draining keeps
        // the loop running (`link_lost_at`), so the host's screen has to be
        // handed back explicitly or the next host inherits it.
        let mut vmu_host_holds = false;
        // The saving indication (a deferred goodbye must show why it waits):
        // the status LED's saving state and a
        // disk icon composited onto every outgoing frame while the queue
        // drains. `was_draining` is the edge detector. The icon changes what
        // is on the LCD without changing what is in the framebuffer, so the
        // frame has to be re-sent at both edges, and — because the send is
        // unacknowledged and ~64% of frames are dropped by the VMU's CRC under
        // BLE traffic — re-sent on the animation interval until it can be
        // presumed to have landed: for the whole drain, and for
        // `VMU_ICON_SETTLE_POLLS` after it ends. Without the tail a dropped
        // final frame leaves the icon on a static screen with nothing to
        // redraw it. The retry is measured from the
        // last frame actually sent (`vmu_polls_since_send`, zeroed at the
        // write), not on a clock of its own: a second clock out of phase with
        // the animation's asked for a frame every 10 polls instead of 20
        // (reproduced by counter simulation). Measured
        // from the send, a retry fires only when the screen has been quiet
        // for an interval, so under the animation it never fires at all.
        #[expect(
            clippy::items_after_statements,
            reason = "the constant is declared beside the loop that consumes it; hoisting it to module scope would separate a tuning value from the only code it tunes"
        )]
        const VMU_ICON_SETTLE_POLLS: u16 = 2 * VMU_HOME_POLLS;
        let mut was_draining = false;
        let mut vmu_polls_since_send: u16 = 0;
        let mut vmu_icon_settle: u16 = 0;
        // When the BLE link dropped while this loop stayed to drain the VMU
        // storage queue; `None` while connected. See the disconnect branch.
        let mut link_lost_at: Option<Instant> = None;
        // Seeded from the shared snapshot, not a placeholder. This used to start
        // at 100 %, so a detect that had just read 25 % handed the VMU a
        // fabricated full battery until Phase 3's own first sample, up to a
        // minute later. `None` = not known, or no longer
        // current — and then the icon is hidden rather than drawn with a guess.
        //
        // USB VBUS, polled far more often than the 60 s gauge cadence. This is
        // what actually drives the charge indicator: the IP5306's `charging` bit
        // drops the moment the pack tops off, so on its own it made "plugged in
        // and full" indistinguishable from "not plugged in" — which is how the
        // bolt came to look broken. VBUS is a hardware line and answers the
        // question the user is actually asking. Read once here rather than
        // assumed absent, so the seed below is the shown level for the cable
        // state the unit is actually in.
        let mut vmu_usb_present = pulsar_dreamcast_ble::usb_vbus_present();
        // The shown level, not the measured one: this feeds the VMU icon and
        // the LED bar, never a cutoff.
        let mut vmu_battery_percent: Option<u8> = battery.snapshot().shown_percent(vmu_usb_present);
        // Tracked separately from `vmu_battery_percent` rather than encoded into
        // it. This used to be smuggled in as `percent = 100`, which made charging
        // and finished-charging render identically — plugging in appeared to do
        // nothing at all when the pack was already near full.
        let mut vmu_battery_charging = battery.snapshot().charging == Some(true);
        let mut last_vbus_check = Instant::now();
        let mut last_state: Option<ControllerState> = None;
        let mut fail_count: u16 = 0;
        let mut last_activity = Instant::now();

        // Goodbye state machine. Activated when GOODBYE_PENDING is set (button
        // task signals at the 7s hold mark, *during* the hold). We render BYE
        // through the existing dirty-flag path so it gets the same radio-idle
        // waiting and retry behavior as the regular pulsar/splash writes.
        // Once the write lands, we hold for ≥1s before triggering System Off
        // (XIAO) or halting via WFI (DK) so the user actually sees BYE.
        #[expect(
            clippy::items_after_statements,
            reason = "the goodbye state machine is declared beside the loop that drives it; it has no other user"
        )]
        #[derive(Clone, Copy)]
        enum GoodbyeState {
            Render,        // need to swap framebuffer to BYE
            Wait(Instant), // BYE in framebuffer, waiting for write or timeout
            Hold(Instant), // BYE on LCD, holding before sleep
        }
        // Maximum time to wait for the dirty flag to clear before giving up
        // and proceeding to Hold anyway. write_vmu_lcd() can return false
        // (no Ack) even when the LCD bytes landed — typically because BLE
        // radio interference corrupted the controller's reply. Without this
        // fallback, the goodbye state machine would loop in Wait forever and
        // never reach enter_system_off().
        const GOODBYE_WAIT_TIMEOUT_MS: u64 = 500;
        let mut goodbye_state: Option<GoodbyeState> = None;

        loop {
            // Fixed reference point for the pacer and the overrun detector
            // at the bottom, and for the poll-period HID channel.
            let iter_start = Instant::now();
            // Time this iteration spent waiting for quiet-window edges
            // (VMU probe path) — excluded from the body budget below.
            let mut align_extra = Duration::from_ticks(0);
            #[cfg(feature = "poll-period-debug")]
            pulsar_dreamcast_ble::poll_period::mark_loop_top();

            {
                let pending = pulsar_dreamcast_ble::GOODBYE_PENDING
                    .load(core::sync::atomic::Ordering::Relaxed);
                // Deferred while acked saves are still draining onto the VMU
                // — sleeping now would lose them, and the reboot would move
                // the generation so the dongle could not replay them. The
                // 15 s hold overrides: `SHUTDOWN_FORCED` is the person's
                // decision that the drain is not worth waiting for. The
                // flag stays set through the deferral, so the goodbye starts
                // on the first pass after the queue empties.
                let forced = pulsar_dreamcast_ble::SHUTDOWN_FORCED
                    .load(core::sync::atomic::Ordering::Relaxed);
                if pending
                    && goodbye_state.is_none()
                    && (forced || !pulsar_dreamcast_ble::ble::host_vmu::draining())
                {
                    log!("MAIN: Goodbye, rendering BYE");
                    goodbye_state = Some(GoodbyeState::Render);
                }
                match goodbye_state {
                    Some(GoodbyeState::Render) => {
                        vmu_framebuf = pulsar_dreamcast_ble::vmu::build_message_splash(b"BYE");
                        vmu_frame_dirty = true;
                        goodbye_state = Some(GoodbyeState::Wait(Instant::now()));
                    }
                    Some(GoodbyeState::Wait(wait_start)) => {
                        if !vmu_frame_dirty
                            || wait_start.elapsed()
                                >= Duration::from_millis(GOODBYE_WAIT_TIMEOUT_MS)
                        {
                            // Either the standard write path cleared the
                            // dirty flag (BYE actually landed on the LCD),
                            // or we've waited long enough that we should
                            // proceed regardless. write_vmu_lcd() can return
                            // false even when the bytes landed — its Ack
                            // gets corrupted by BLE radio events during
                            // notify activity. Without this timeout the
                            // state machine could loop here forever.
                            log!("MAIN: BYE rendered, holding then System Off");
                            goodbye_state = Some(GoodbyeState::Hold(Instant::now()));
                        }
                    }
                    Some(GoodbyeState::Hold(start))
                        if start.elapsed() >= Duration::from_millis(1000) =>
                    {
                        // Re-asked here, not only at entry: the link stays up
                        // through the BYE render and this hold, and the BLE
                        // task goes on acking WRITEs into the queue the whole
                        // time. No `await` separates this
                        // check from the sleep, and the BLE task cannot run
                        // between them, so what it says is what is true at
                        // System Off. If saves arrived, step back to the
                        // deferral — BYE stays on the LCD, and the goodbye
                        // restarts the pass after the queue empties.
                        if forced || !pulsar_dreamcast_ble::ble::host_vmu::draining() {
                            log!("MAIN: goodbye hold done — entering sleep");
                            // SAFETY: `sleep_now` requires an initialised SoftDevice and never returns.
                            // Both hold here: the SoftDevice is enabled during setup, well before this
                            // point, and this call diverges — nothing after it can observe the
                            // torn-down pin state.
                            unsafe {
                                sleep_now(&mut power, &mut status);
                            }
                        }
                        log!("MAIN: goodbye deferred — saves arrived during the hold");
                        goodbye_state = None;
                        // BYE is in the framebuffer, and nothing below would
                        // replace it: a host's hold keeps the animation off,
                        // and the splash window redraws nothing — so BYE plus
                        // the saving icon would sit there for the whole drain.
                        // Hand the redraw to the
                        // hold-over path, which restores the profile splash or
                        // the animation in this pass. A game that is still
                        // drawing retakes the screen with its next frame; a
                        // parked game's art comes back as the animation until
                        // it draws again — keeping a copy to put back would be
                        // 192 B in this task's future, and RAM there moves the
                        // layout (v267), for a case that needs a WRITE to land
                        // inside the goodbye's 1 s hold.
                        vmu_host_holds = false;
                        vmu_home_polls = 1;
                    }
                    // Resend BYE through the hold. The write is unacked, so a
                    // clear dirty flag only says it went out, not that it
                    // landed — and the VMU's CRC drops most frames under BLE
                    // traffic. One send left BYE to chance; this is the retry
                    // the home glyph and CHRG already get, measured from the
                    // last send so it cannot double up.
                    Some(GoodbyeState::Hold(_)) if vmu_polls_since_send >= VMU_ANIM_INTERVAL => {
                        vmu_frame_dirty = true;
                    }
                    Some(GoodbyeState::Hold(_)) | None => {}
                }
            }

            // Check for BLE disconnect.
            //
            // A drop is two things, and they come apart here. The *host* is
            // gone the moment the state leaves `Connected`: its read, its
            // screen and its rumble go at once, whatever happens next. The
            // *card* is not gone, and if the dongle has acked saves staged
            // for it the loop stays — rail up, presence probed, drain running
            // — until the queue is empty, the deadline passes, the person
            // forces a shutdown, or the drop turns out to be a sync press.
            // Only then does the loop leave, and leaving is what ends the
            // generation: a port nobody is watching counts as changed.
            let conn_state = get_connection_state();
            if conn_state == ConnectionState::Connected {
                if link_lost_at.take().is_some() {
                    log!("MAIN: BLE back mid-drain — same generation, carrying on");
                }
            } else {
                if link_lost_at.is_none() {
                    link_lost_at = Some(Instant::now());
                    // The read side belongs to the host that left: cancel the
                    // schedule (which releases any held LCD frame) and drop the
                    // host's queue, decode in flight and latched capture — none
                    // of it has a consumer left, and none of it may be served
                    // as an answer to whoever connects next. The write queue,
                    // the generation, and the writer's read-back in flight are
                    // the card's, and stay.
                    pulsar_dreamcast_ble::ble::host_vmu::link_down();
                    #[cfg(feature = "spim-capture")]
                    {
                        read_sched.note_disconnect(Instant::now().as_ticks());
                        reader.clear_host();
                    }
                    // The host's screen and its LCD ingress go back with it —
                    // the outer loop's `host_lcd::reset` at the next session
                    // entry is not reached while this loop stays to drain.
                    pulsar_dreamcast_ble::ble::host_lcd::reset();
                    vmu_host_holds = false;
                    // Stop the motor now, and drop any command the host queued
                    // but we never applied. The rail stays up through the drain,
                    // the grace and sync, so this is the only thing
                    // that silences the motor — it must stay unconditional.
                    rumble.set(0);
                    pulsar_dreamcast_ble::RUMBLE_LEVEL.reset();
                    RAW_CONTROLLER_STATE.signal(ControllerState::default());
                    pulsar_dreamcast_ble::MAPLE_START_HELD
                        .store(false, core::sync::atomic::Ordering::Relaxed);
                }
                let forced = pulsar_dreamcast_ble::SHUTDOWN_FORCED
                    .load(core::sync::atomic::Ordering::Relaxed);
                let hold = conn_state != ConnectionState::SyncMode
                    && !forced
                    && pulsar_dreamcast_ble::ble::host_vmu::draining()
                    && link_lost_at
                        .is_some_and(|at| at.elapsed().as_millis() < LINK_LOSS_DRAIN_DEADLINE_MS);
                if hold {
                    // The lease holds. Everything below this branch runs as if
                    // connected: presence is probed, the drain takes its
                    // windows, the HID signal goes to a task with nobody to
                    // send it to.
                } else {
                    // If the disconnect is because we just entered sync mode, write
                    // a SYNC splash to the VMU so it persists through Phase 1 — and
                    // leave the rail up so it actually can (see `RailHold::Sync`);
                    // Phase 1 drops it when the sync window ends.
                    if conn_state == ConnectionState::SyncMode {
                        rail_hold = RailHold::Sync;
                        // This window's splash is this one; Phase 1 skips its own.
                        sync_splash_tried = true;
                        if vmu_known_absent(&host, &mut bus) {
                            // The controller says the slot is empty: nothing to
                            // draw on, so spend no retries on it.
                            log!("MAIN: Sync mode entered, no VMU — no SYNC splash");
                        } else {
                            log!("MAIN: Sync mode entered, writing SYNC splash");
                            let mut send_buf =
                                pulsar_dreamcast_ble::vmu::build_message_splash(b"SYNC");
                            pulsar_dreamcast_ble::vmu::rotate_180(&mut send_buf);
                            // Retried until acked: one unchecked attempt on the
                            // bit-bang path left the old frame up when an
                            // interrupt corrupted it. The helper enumerates before
                            // every attempt, unconditionally — gating that on
                            // `vmu_present` would make the splash depend on the
                            // poll loop having enumerated within the last 3 s. Its
                            // result is not assigned to `vmu_present` either:
                            // presence belongs to the probe's `sub_peripheral_mask`,
                            // and nothing reads it after this. Warm budget, ~1 s
                            // only if the VMU never acks; stops early for a
                            // goodbye, DFU or sleep request.
                            let outcome = vmu_splash_until_acked(
                                &mut bus,
                                &host,
                                &send_buf,
                                VMU_SPLASH_ATTEMPTS,
                                sync_splash_superseded,
                            )
                            .await;
                            if !outcome.acked {
                                log!("MAIN: SYNC write not acked (may still have drawn)");
                            }
                        }
                    }
                    if pulsar_dreamcast_ble::ble::host_vmu::draining() {
                        log!("MAIN: leaving with saves still staged — discarding them");
                    }
                    log!("MAIN: BLE disconnected, leaving poll loop");
                    // Leaving is the end of the generation: whether the rail is
                    // held for the grace or for the sync splash,
                    // nothing probes the port from Phase 1, and unobserved
                    // counts as changed.
                    // Staged blocks become `WRITTEN DISCARDED`, delivered to the
                    // next session.
                    pulsar_dreamcast_ble::ble::host_vmu::unwatched();
                    if rail_hold != RailHold::Sync {
                        rail_hold = RailHold::after_drop(link_lost_at.unwrap_or_else(Instant::now));
                    }
                    status.off();
                    break;
                }
            }

            // Apply any pending rumble command from the host (HID output report).
            if let Some(level) = pulsar_dreamcast_ble::RUMBLE_LEVEL.try_take() {
                rumble.set(level);
            }

            // OTA DFU handoff from the button task. The reset happens here so
            // a BOOT splash can land first, between polls, on a quiet bus. The
            // LCD keeps its last frame while dock power holds, so the splash
            // stays up through DFU mode as the "updating" indicator — on
            // pulsarv1 the 5V rail is the IP5306 boost, up because we are in
            // Phase 3, and an MCU reset doesn't touch it; the app's own init
            // drops it after the update. (XIAO's discrete boost-enable pin goes
            // hi-Z at reset, so there the rail — and the splash — may drop;
            // retail hardware is pulsarv1.) Deliberately no rail_off here.
            let dfu_requested = pulsar_dreamcast_ble::DFU_PENDING
                .swap(false, core::sync::atomic::Ordering::Relaxed);
            // Refused outright while acked saves are draining onto the VMU: the
            // bootloader reboot loses the queue and moves the generation.
            // Consumed, not deferred — a reboot landing seconds after the
            // gesture, once the person has moved on, is worse than asking them
            // to try again after the drain.
            let dfu_refused = dfu_requested && pulsar_dreamcast_ble::ble::host_vmu::draining();
            if dfu_refused {
                log!("MAIN: DFU refused — saves still draining to the VMU");
                // Without this the refusal is invisible but for the missing
                // flash. Rides the hold the CHRG splash below uses.
                //
                // Not while a host owns the screen, for either splash: the
                // hold never counts down there (the host branch outranks it
                // in the frame logic), so nothing would restore the host's
                // art, and a parked game would keep the splash until it next
                // drew. The disk icon still says why.
                if !vmu_host_holds {
                    vmu_framebuf = pulsar_dreamcast_ble::vmu::build_message_splash(b"SAVE");
                    vmu_frame_dirty = true;
                    vmu_home_polls = VMU_HOME_POLLS * 2; // ~2s
                }
            }
            if dfu_requested && !dfu_refused {
                // One shared gate for all three sites — see `dfu_battery_refusal`.
                if let Some(_verdict) = dfu_battery_refusal(&mut power).await {
                    log!(
                        "MAIN: DFU refused: {:?} (need {}% or charger) — showing CHRG",
                        _verdict,
                        DFU_MIN_BATTERY_PCT
                    );
                    // Ride the home-glyph hold mechanism: swap WHICH frame the
                    // normal write path sends and let the counter restore the
                    // underlying content afterward — no extra bus traffic. The
                    // flag was consumed by the swap above, so the gesture can
                    // simply be retried (on a charger) after release.
                    if !vmu_host_holds {
                        vmu_framebuf = pulsar_dreamcast_ble::vmu::build_message_splash(b"CHRG");
                        vmu_frame_dirty = true;
                        vmu_home_polls = VMU_HOME_POLLS * 2; // ~2s
                    }
                } else if !pulsar_dreamcast_ble::ble::host_vmu::pause_writes_unless_draining() {
                    // Asked again: the battery read above awaits I²C off USB,
                    // and the BLE task can accept and ack a block in that
                    // await. Passing also closes write admission, because the
                    // BOOT splash below still yields (its retry waits), and a
                    // save acked there would be lost to the reboot.
                    log!("MAIN: DFU refused — a save arrived during the battery check");
                    // The button task flashed "taken" before this save
                    // arrived, so the splash is the only sign of the refusal.
                    if !vmu_host_holds {
                        vmu_framebuf = pulsar_dreamcast_ble::vmu::build_message_splash(b"SAVE");
                        vmu_frame_dirty = true;
                        vmu_home_polls = VMU_HOME_POLLS * 2; // ~2s
                    }
                } else {
                    log!("MAIN: DFU pending — BOOT splash, then OTA bootloader");
                    let mut send_buf = pulsar_dreamcast_ble::vmu::build_boot_splash(
                        pulsar_dreamcast_ble::installed_app_version(),
                    );
                    pulsar_dreamcast_ble::vmu::rotate_180(&mut send_buf);
                    // Retried like the SYNC splash: BOOT is the only indicator
                    // through the whole update, and one bit-bang write can be
                    // corrupted by an interrupt. The helper enumerates before
                    // each attempt (the VMU refuses BLOCK_WRITE until asked for
                    // device info). Bounded at ~1 s, and skipped when the
                    // controller says the slot is empty — a missing or deaf VMU
                    // must not hold up the reboot beyond that.
                    if !vmu_known_absent(&host, &mut bus) {
                        let _ = vmu_splash_until_acked(
                            &mut bus,
                            &host,
                            &send_buf,
                            VMU_SPLASH_ATTEMPTS,
                            never_superseded,
                        )
                        .await;
                    }
                    pulsar_dreamcast_ble::reboot_into_ota_dfu();
                }
            }

            // the pipeline's liveness step, unconditional and ahead
            // of the schedule. A latched capture that nothing applies strands
            // the pipeline silently — `decoding` false stops the slack loop and
            // `pipeline_busy` true stops the scheduler — so this is asked every
            // window rather than only when something looks like it changed. See
            // `BlockReader::service`. It is three branch tests when idle.
            #[cfg(feature = "spim-capture")]
            reader.service();

            // the host service's turn at the window head, in the
            // order the pipeline wants it.
            //
            // Presence and idleness are *published* here rather than at each
            // site that changes them. Both are set from several places in this
            // loop — presence from the probe, from a controller loss and from
            // teardown — and a publish per window cannot miss one; the setters
            // return immediately when nothing moved.
            #[cfg(feature = "spim-capture")]
            {
                use maple_protocol::read_pipeline::Owner;
                use maple_protocol::write_seq::Verdict;
                use pulsar_dreamcast_ble::ble::host_vmu;
                host_vmu::set_vmu_present(vmu_present);
                host_vmu::set_idle(idle_watch.idle_at(iter_start.as_ticks()));

                // The writer's block is its own only while the service says
                // so. The generation can end under a write — undock, the
                // absence streak, a refused write — and the slot is then
                // `DISCARDED` behind the writer's back; writing on would put a
                // stale block onto whatever card is docked now.
                if let Some(block) = writer.block() {
                    if host_vmu::writing() != Some(block) {
                        log!("VMUIO: block {} no longer the writer's — abandoned", block);
                        writer.abandon();
                    }
                }

                // Taking the decoded block first: it is what releases the
                // pipeline's backpressure, and an unread `ready` stops the
                // next capture being applied. A block goes to whoever asked
                // for it — the dongle, or the writer checking its own work.
                if let Some(ready) = reader.take_ready() {
                    match ready.owner {
                        Owner::Host => host_vmu::publish_block(ready.block, ready.data),
                        Owner::Writer => writer.note_read_back(ready.block, ready.data),
                    }
                }
                // Both owners' failures, not one: the pipeline holds one per
                // owner, and a decode ending can fail the host's block and
                // the writer's read-back in the same window.
                while let Some((block, stage, owner)) = reader.take_failed() {
                    match owner {
                        Owner::Host => host_vmu::publish_failure(block, stage),
                        Owner::Writer => writer.note_read_back_failed(block),
                    }
                }

                // The writer's turn: a read handed to the reader when one
                // is due — the read-back, or the guard's fingerprint blocks
                // — and the verdict on the block in hand once there is one.
                // `FAILED` and `Unidentified` both end the generation inside
                // the service. A guard pass is the service's to remember: it
                // vouches for the card until the port is next enumerated
                // outside the guard, so a drain is guarded once.
                if writer.take_guard_pass() {
                    log!("VMUIO: card identified — the one the dongle pulled");
                    host_vmu::note_card_verified();
                }
                if let Some((block, verdict)) = writer.service(&mut reader) {
                    match verdict {
                        Verdict::Written => host_vmu::publish_written(block),
                        // Ending the generation discards the host's READ in the
                        // service, but not the capture the pipeline holds for
                        // it. Left there, it could answer the host's re-ask
                        // under the new epoch with the old card's block. The
                        // writer's own read was consumed to reach this verdict.
                        Verdict::Failed => {
                            host_vmu::publish_write_failed(block);
                            reader.clear_host();
                        }
                        Verdict::Unidentified => {
                            host_vmu::publish_unidentified(block);
                            reader.clear_host();
                        }
                    }
                }
                // Then the next block, in drain order, if the writer is free
                // and a card is docked. Not idle-gated: a save the console
                // has already committed lands during play, paced by the
                // schedule's cadence like a read. The fingerprint the guard
                // compares against and whether it has to run are read here,
                // in the same window, so the block and its expectation
                // cannot belong to different generations.
                if !writer.busy() {
                    if let Some(block) = host_vmu::take_write(writer.data_mut()) {
                        let verified = host_vmu::card_verified();
                        log!(
                            "VMUIO: writing block {}{}",
                            block,
                            if verified { "" } else { " — guard first" }
                        );
                        writer.begin(block, host_vmu::expected(), verified);
                    }
                }

                // Input resumed under an issued read: take it back and drop
                // whatever the pipeline was holding for it. The request itself
                // survives and is re-issued when the pad settles — the dongle
                // asked once and is owed one answer. The writer's read-back,
                // if one is in flight, is not the host's and stays.
                if host_vmu::recall() {
                    reader.clear_host();
                }
                // One outstanding read, so this hands over at most one block
                // and only while the pad is idle. `take_request` is the whole
                // of the idle-only rule from the queue's side.
                if let Some(block) = host_vmu::take_request() {
                    if !reader.request(block) {
                        // A full queue cannot happen with one outstanding
                        // request, and if it ever does the request stays
                        // unissued rather than being silently lost.
                        log!("VMUIO: read queue refused block {}", block);
                        host_vmu::recall();
                    }
                }
            }

            // A scheduled storage transaction — a block read, or one of the
            // writer's phases — takes this window *instead of* the controller
            // poll: the two never share one, because each is a reply-bearing
            // capture and the quiet window fits one. The schedule is asked at
            // every head, before anything is transmitted, and the writer goes
            // first when both want the slot.
            #[cfg(feature = "spim-capture")]
            let read_window = {
                use maple_protocol::read_sched::{Ctx, Slot};
                let gap_ticks = lcd_end.map_or(maple_protocol::read_sched::GAP_NONE, |t| {
                    u32::try_from(iter_start.saturating_duration_since(t).as_ticks())
                        .unwrap_or(maple_protocol::read_sched::GAP_NONE)
                });
                let ctx = Ctx {
                    now: iter_start.as_ticks(),
                    n: READ_EVERY_N,
                    vmu_present,
                    gap_ticks,
                    pipeline_busy: reader.pipeline_busy(),
                    // Named, not inferred: an empty queue means nothing is owed,
                    // so an owed read is cancelled and the display released. See
                    // `BlockReader::work`. A writer with a transaction owed is
                    // work remaining whatever the reader's queue says.
                    work: if writer.wants_window() {
                        maple_protocol::read_sched::ReadWork::Remaining
                    } else {
                        reader.work()
                    },
                };
                read_sched.window(&ctx) == Slot::Read
            };
            #[cfg(not(feature = "spim-capture"))]
            let read_window = false;

            #[cfg(feature = "poll-timing")]
            let _pt_gc = pulsar_dreamcast_ble::poll_timing::start();
            #[cfg(feature = "poll-period-debug")]
            let _pp_gc = pulsar_dreamcast_ble::poll_period::stamp();
            let gc_result = if read_window {
                #[cfg(feature = "spim-capture")]
                if writer.wants_window() {
                    writer.write_window(&mut bus, &mut read_sched, iter_start.as_ticks());
                } else {
                    reader.read_window(&mut bus, &mut read_sched, iter_start.as_ticks());
                }
                // No fresh controller sample this window: BLE keeps notifying
                // the last one. Deliberately not counted as a failed poll — the
                // `else` arm below is guarded on `!read_window` for that — and
                // not counted as a poll either: tag 7 is the denominator every
                // capture-health rate is taken out of, and a window that never
                // armed the capture does not belong in it.
                MapleResult::Timeout
            } else {
                #[cfg(feature = "poll-period-debug")]
                pulsar_dreamcast_ble::poll_period::note_poll();
                let r = host.get_condition(&mut bus);
                // Immediately after, before anything else can drive the bus:
                // the no-trigger abort is attributed to *this* poll by the
                // counter having moved, which only holds while nothing else has
                // armed the capture in between.
                #[cfg(feature = "poll-period-debug")]
                pulsar_dreamcast_ble::poll_period::note_poll_end();
                r
            };
            #[cfg(feature = "poll-timing")]
            pulsar_dreamcast_ble::poll_timing::record_gc(_pt_gc);
            #[cfg(feature = "poll-period-debug")]
            pulsar_dreamcast_ble::poll_period::record_gc(_pp_gc);
            if let MapleResult::Ok(state) = gc_result {
                if fail_count >= CONTROLLER_LOST_THRESHOLD {
                    log!("MAPLE: Controller reconnected");
                }
                fail_count = 0;

                // Mirror Start for the button task's DFU gesture — every poll,
                // not just on change, so it tracks the live held state.
                pulsar_dreamcast_ble::MAPLE_START_HELD
                    .store(state.buttons.start, core::sync::atomic::Ordering::Relaxed);

                // Publish the raw source sample on every successful poll. The
                // remapper/config LiveInput path must see small analog changes
                // even when the current HID inactivity filter considers them
                // noise. `Signal` is intentionally a one-slot latest-value
                // latch, so a faster producer simply replaces an unread sample.
                RAW_CONTROLLER_STATE.signal(state);

                // the idle gate, fed only from *fresh* samples: a
                // window that read no controller state must not age the clock
                // forward on stale input, or a read window every second poll
                // would look like a second of stillness.
                #[cfg(feature = "spim-capture")]
                let _idle = idle_watch.note(&state, iter_start.as_ticks());

                #[expect(
                    clippy::option_if_let_else,
                    reason = "the match reads as the first-poll-vs-subsequent distinction it is; map_or would bury both arms in a closure"
                )]
                let changed = match &last_state {
                    None => true,
                    Some(prev) => prev.state_changed(&state),
                };

                // Keep the filtered comparison only for inactivity tracking.
                // Do not update `last_state` on every raw sample: a slowly
                // moving axis could otherwise remain below the delta threshold
                // forever.
                if changed {
                    last_state = Some(state);
                    last_activity = Instant::now();
                }

                // VMU content: profile splash for the first 30s of every boot,
                // then the rotating pulsar (with battery overlay) at ~6fps.
                // Skipped entirely while goodbye is active so the BYE frame
                // doesn't get overwritten before it lands.
                //
                // History note: the animation was removed on 2026-06-11 when
                // VMU writes were believed to cause SoftDevice asserts. The
                // real cause was the diagnostic instrumentation masking
                // interrupts (see poll_timing module docs); the writes were
                // innocent. With the PWM/EasyDMA TX (~1.7ms hardware-timed
                // wire frames, CPU awaits during playback) the animation
                // costs ~0.5ms of average poll period and cannot corrupt the
                // bus or perturb the controller.
                let vmu_busy = goodbye_state.is_some();
                // Saving indication. While acked saves are draining onto the
                // card the status LED goes amber and a disk sits in the LCD's
                // top-left corner, over whatever is on screen — a game's art
                // included, since that is when a person is least expecting a
                // drain — so someone reaching for the dock, or holding the
                // button and not getting BYE (the goodbye defers above), can
                // see why. Asked once per window here; the sites that act on
                // the answer — the goodbye, the DFU gesture, the sleep — ask
                // again at the moment they decide, since saves can arrive
                // in between. The icon is drawn at the write below, not here:
                // the framebuffer is never touched, so nothing has to be put
                // back when it goes.
                let draining_now = pulsar_dreamcast_ble::ble::host_vmu::draining();
                if draining_now != was_draining {
                    was_draining = draining_now;
                    if draining_now {
                        log!("MAIN: saves draining — saving icon up");
                        status.saving();
                    } else {
                        log!("MAIN: drain over — saving icon down, LED green");
                        status.connected();
                    }
                    // What the LCD shows changed; what the framebuffer holds
                    // did not. Send it again, and keep sending it.
                    vmu_frame_dirty = true;
                    vmu_icon_settle = VMU_ICON_SETTLE_POLLS;
                }
                // The retry: through the drain and the settle tail after it,
                // re-send the frame whenever the screen has been quiet for an
                // animation interval, whatever the frame holds. Traffic on top
                // of what the screen was making only when that was static — a
                // splash, a parked game, the home glyph; under the animation
                // the screen is never quiet that long and this never fires.
                // Not while the goodbye owns the screen: its Wait state reads
                // the dirty flag as "BYE landed", and BYE outranks the icon in
                // any case.
                vmu_polls_since_send = vmu_polls_since_send.saturating_add(1);
                if !vmu_busy && (draining_now || vmu_icon_settle > 0) {
                    vmu_icon_settle = vmu_icon_settle.saturating_sub(1);
                    if vmu_polls_since_send >= VMU_ANIM_INTERVAL {
                        vmu_frame_dirty = true;
                    }
                }
                // Host frames first: they outrank every local content source
                // while a game is drawing. `take_frame` hands over only the
                // newest complete frame and drops whatever arrived in between,
                // so nothing queues and a fast sender costs nothing but the
                // frames it skipped.
                let mut vmu_host_owned = vmu_host_holds;
                let mut vmu_host_frame = false;
                if !vmu_busy {
                    if vmu_host_wait > 0 {
                        vmu_host_wait -= 1;
                    } else if pulsar_dreamcast_ble::ble::host_lcd::take_frame(&mut vmu_framebuf) {
                        vmu_host_frame = true;
                        vmu_host_owned = true;
                        vmu_frame_dirty = true;
                        vmu_host_wait = VMU_HOST_INTERVAL;
                        vmu_host_holds = true;
                        // A game is on the screen, so the boot splash's window
                        // is over — it must not come back underneath when the
                        // hold-off expires. This also leaves the animation as
                        // the only content the hold-off can resume to, which is
                        // what makes that branch a two-liner.
                        vmu_splash_polls = 0;
                        vmu_home_polls = 0;
                    }
                }
                // Best-effort Guide-chord home glyph. Consume the one-shot flag
                // (only when not mid-goodbye) and swap in the house icon; the
                // hold counter keeps it on-screen for ~1s. This only changes
                // WHICH frame the existing single write sends — no extra bus
                // traffic, so Maple timing is untouched. If a chord fires during
                // goodbye it's simply dropped (device is shutting down).
                //
                // The flag is consumed whether or not the glyph is drawn: while
                // a game owns the screen the chord's feedback is dropped rather
                // than painted over the game's art, and a dropped one must not
                // fire seconds later when the host goes quiet.
                let guide_glyph = !vmu_busy
                    && pulsar_dreamcast_ble::GUIDE_GLYPH_PENDING
                        .swap(false, core::sync::atomic::Ordering::Relaxed);
                if guide_glyph && !vmu_host_owned {
                    vmu_framebuf = pulsar_dreamcast_ble::vmu::build_home_splash();
                    vmu_frame_dirty = true;
                    vmu_home_polls = VMU_HOME_POLLS;
                }
                if vmu_busy {
                    // Goodbye in flight — leave vmu_framebuf alone.
                } else if vmu_host_frame {
                    // The host frame is already in vmu_framebuf and marked
                    // dirty; nothing local gets a say this poll.
                } else if vmu_host_holds {
                    // The host owns the screen and keeps it. Nothing local gets
                    // a say, and no frame is redrawn: the VMU's LCD holds what
                    // it was last given for as long as dock power does. The
                    // branch has to stay even though it is empty, so ownership
                    // still takes precedence over the home glyph below.
                } else if vmu_home_polls > 0 {
                    vmu_home_polls -= 1;
                    // Re-mark dirty on the animation interval so the held static
                    // frame (house glyph, or the DFU-refusal CHRG splash) retries
                    // past the ~64% CRC-collision drop rate and reliably lands.
                    if vmu_home_polls.is_multiple_of(VMU_ANIM_INTERVAL) {
                        vmu_frame_dirty = true;
                    }
                    if vmu_home_polls == 0 {
                        // Hold over: explicitly redraw the underlying content
                        // *now* so the held frame never lingers. The splash/animation
                        // branches below only re-render on their own schedule, so
                        // relying on them would freeze the house on-screen (the
                        // splash branch doesn't redraw at all). Restore the boot
                        // profile splash if still in its window, else the pulsar.
                        if vmu_splash_polls > 0 {
                            vmu_framebuf = pulsar_dreamcast_ble::vmu::build_profile_splash(
                                profile.vmu_glyph,
                                profile.vmu_label,
                            );
                        } else {
                            vmu_framebuf =
                                pulsar_dreamcast_ble::vmu::build_animated_frame(vmu_anim_step);
                            vmu_anim_step =
                                (vmu_anim_step + 1) % pulsar_dreamcast_ble::vmu::ROTATION_FRAMES;
                            vmu_anim_counter = 0;
                        }
                        vmu_frame_dirty = true;
                    }
                } else if vmu_delay > 0 {
                    vmu_delay -= 1;
                } else if vmu_splash_polls > 0 {
                    vmu_splash_polls -= 1;
                    if vmu_splash_polls == 0 {
                        // Transition out of splash: prime the animation so the
                        // first pulsar frame renders on the next interval.
                        vmu_anim_counter = VMU_ANIM_INTERVAL;
                    }
                } else {
                    vmu_anim_counter += 1;
                    if vmu_anim_counter >= VMU_ANIM_INTERVAL {
                        vmu_framebuf =
                            pulsar_dreamcast_ble::vmu::build_animated_frame(vmu_anim_step);
                        vmu_anim_step =
                            (vmu_anim_step + 1) % pulsar_dreamcast_ble::vmu::ROTATION_FRAMES;
                        vmu_anim_counter = 0;
                        vmu_frame_dirty = true;
                    }
                }

                // VMU write: fire-and-forget, unanchored. Radio notifications
                // are NOT used: every gate built on them (alternating-flag,
                // INT_ON_INACTIVE, gap-classified ON_BOTH) produced SoftDevice
                // assertion panics whenever writes were active, while no-write
                // runs were clean (debug log 2026-06-10, five rounds). The
                // unanchored cost is known and bounded: ~64% of frames collide
                // with a connection event and are dropped by the VMU's CRC
                // (the LCD keeps the previous frame), so the 6fps animation
                // renders at ~2fps effective. Battery overlay is composited
                // here so every frame gets it regardless of content source.
                if last_vmu_probe.is_none_or(|t| t.elapsed() >= VMU_PROBE_INTERVAL) {
                    let was_present = vmu_present;
                    // Presence comes from the CONTROLLER's device-info reply: a
                    // main peripheral ORs a bit into its own sender address for
                    // each attached sub-peripheral (0x20 bare, 0x21 with a VMU
                    // in slot 1). One transaction, to the controller, answers
                    // for every slot.
                    //
                    // The old `enumerate_vmu`-based gate was dead when it was
                    // replaced: the VMU's reply at 0x01 did not decode before
                    // `e8ee520`'s `find_data_start`. It decodes now (20/20 on the
                    // 2026-09-11 bench); the controller's reply stays
                    // the source because it needs no VMU transaction.
                    //
                    // Own quiet window: get_condition already spent most of
                    // this one, and a probe started in the tail collides with
                    // the next connection event essentially every time (the
                    // soak's VMU-presence flap). One probe fits a window head
                    // comfortably (~6-8ms of ~12).
                    align_extra += align_to_quiet_window(EXTRA_ALIGN_CAP_MS).await;
                    let mask = host.sub_peripheral_mask(&mut bus);
                    // `Some` means the controller's device-info reply decoded, so
                    // its sub-peripheral bits are authoritative either way — that,
                    // not `detected`, is what makes `vmu_present` meaningful.
                    presence_known |= mask.is_some();
                    let detected = mask.is_some_and(|m| {
                        m & pulsar_dreamcast_ble::maple::host::addressing::SUB_SLOT_1 != 0
                    });
                    if detected {
                        vmu_probe_misses = 0;
                        vmu_present = true;
                        // Re-enumerate for its SIDE EFFECT, not its return value.
                        // A freshly docked VMU ignores BLOCK_WRITE — no reply, no
                        // frame — until it has been sent a device-info request
                        // (bench, 2026-09-11), so this request is what
                        // keeps the LCD accepting frames.
                        //
                        // Dropping it is what blanked the screen when presence
                        // moved to the controller's reply: the call looked dead
                        // because its result was always false then (before
                        // `e8ee520`, the reply never decoded), but the TX was
                        // load-bearing. Only sent while a VMU is actually docked,
                        // so an empty bay costs nothing (the old code paid it
                        // unconditionally).
                        //
                        // Cadence: on every undocked→docked transition (a fresh
                        // VMU is un-enumerated) plus every 3rd pass (~15s
                        // re-arm, covers a docked VMU power-cycling). NOT every
                        // pass: this is a third bus transaction needing its own
                        // quiet window, and run #45 showed each extra window is
                        // a conn event with no fresh input.
                        //
                        // And NOT while acked saves are draining onto the card.
                        // A freshly docked VMU answers nothing
                        // until it is asked for device info, and that deafness
                        // is the swap guard's alarm: a card swapped mid-drain
                        // — inside the four-miss window presence needs — must
                        // stay deaf until the writer's guard enumerates it
                        // and reads its fingerprint. Re-arming it here would
                        // hand it the previous card's saves under the same
                        // epoch. The price is the power-cycle recovery for the
                        // length of the drain (≈ 0.6 s a block); the dock
                        // transition still enumerates, because presence
                        // having been lost means the generation has ended and
                        // there is nothing left to drain. An enumeration that
                        // does run tells the service so: the guard looks again
                        // before the next block.
                        vmu_enum_passes = vmu_enum_passes.saturating_add(1);
                        let draining = pulsar_dreamcast_ble::ble::host_vmu::draining();
                        if !was_present || (vmu_enum_passes >= 3 && !draining) {
                            vmu_enum_passes = 0;
                            align_extra += align_to_quiet_window(EXTRA_ALIGN_CAP_MS).await;
                            let _ = host.enumerate_vmu(&mut bus);
                            pulsar_dreamcast_ble::ble::host_vmu::note_enumerated();
                        }
                    } else {
                        vmu_probe_misses = vmu_probe_misses.saturating_add(1);
                        if vmu_probe_misses >= VMU_ABSENT_STREAK {
                            vmu_present = false;
                        }
                    }
                    last_vmu_probe = Some(Instant::now());
                    if vmu_present != was_present {
                        log!("VMU: {}", if vmu_present { "docked" } else { "removed" });
                        // Either direction voids everything in flight: an undock
                        // leaves reads owed against a card that is gone, and a
                        // dock may be a *different* card, whose block 5 is not
                        // the one a latched capture holds. The full identity
                        // check is the swap guard (`write_seq`); this is the part of
                        // it that costs nothing and must not wait for it.
                        #[cfg(feature = "spim-capture")]
                        reader.clear();
                        // The VMU shows battery itself, so the LED gauge is only
                        // lit when it can't.
                        status.set_battery(if vmu_present {
                            None
                        } else {
                            vmu_battery_percent
                        });
                        // A VMU that just appeared has a blank LCD — redraw now.
                        vmu_frame_dirty = vmu_present;
                    }
                }

                // Gated on `vmu_present`: with no VMU docked this would
                // otherwise push a ~1.7ms DMA TX into empty air every ~300ms —
                // bus occupancy and power for nobody.
                //
                // This was previously ungated on purpose, because presence came
                // from a probe that failed constantly and would have frozen the
                // display. That reasoning no longer holds: presence is now read
                // from the controller's device-info reply (the RX path that
                // makes controller detection work), and `VMU_ABSENT_STREAK`
                // requires 4 consecutive misses — 12s — before declaring the bay
                // empty, so no single dropped exchange can blank the screen.
                // Poll VBUS on a 1 s cadence — cheap (one SVC), and unlike the
                // gauge it must feel immediate: plugging in and waiting up to a
                // minute for any acknowledgement is most of what made this look
                // broken. Redraw only on the transition, so an unchanging state
                // costs nothing.
                #[expect(
                    clippy::items_after_statements,
                    reason = "the constant belongs beside the poll it paces; module scope would separate it from its only consumer"
                )]
                const VBUS_POLL_INTERVAL: Duration = Duration::from_millis(1000);
                if last_vbus_check.elapsed() >= VBUS_POLL_INTERVAL {
                    last_vbus_check = Instant::now();
                    let vbus = pulsar_dreamcast_ble::usb_vbus_present();
                    if vbus != vmu_usb_present {
                        vmu_usb_present = vbus;
                        vmu_frame_dirty = true;
                    }
                }

                // an owed-but-blocked read holds the display. Without
                // that, a frame dirty in every window re-arms the gap and the
                // read never becomes eligible — the display starves the read.
                // The frame stays dirty and goes out at the first window whose
                // gate opens.
                #[cfg(feature = "spim-capture")]
                let frame_go =
                    vmu_present && vmu_frame_dirty && read_sched.frame_gate(iter_start.as_ticks());
                #[cfg(not(feature = "spim-capture"))]
                let frame_go = vmu_present && vmu_frame_dirty;
                if frame_go {
                    let mut send_buf = vmu_framebuf;
                    // Not over a host frame: while a game is drawing, the game
                    // owns all 48x32 of it. The overlay comes back
                    // with the animation when the link drops and ownership is
                    // released.
                    if !vmu_host_owned {
                        // Either source counts as "power going in". VBUS is the
                        // one that actually fires; the gauge bit is kept so a
                        // board whose VBUS read fails still shows something while
                        // genuinely charging.
                        let charging_shown = vmu_usb_present || vmu_battery_charging;
                        pulsar_dreamcast_ble::vmu::composite_battery(
                            &mut send_buf,
                            vmu_battery_percent.unwrap_or(0),
                            charging_shown,
                            // An unknown level is not drawn. The bolt needs no
                            // level, so it still shows.
                            charging_shown || vmu_battery_percent.is_some(),
                        );
                    }
                    // The saving icon goes over everything but BYE — the host's
                    // frame included, unlike the battery, because a drain under
                    // a game is exactly when a person needs to be told not to
                    // pull the card. On the copy, so the framebuffer keeps the
                    // art for the frame after the drain.
                    if draining_now && !vmu_busy {
                        pulsar_dreamcast_ble::vmu::composite_saving(&mut send_buf);
                    }
                    pulsar_dreamcast_ble::vmu::rotate_180(&mut send_buf);
                    // Flush pending tasks (HID notify, SoftDevice runner)
                    // before the TX starts; the DMA playback then awaits, so
                    // the executor also runs DURING the TX.
                    embassy_futures::yield_now().await;
                    #[cfg(feature = "poll-timing")]
                    let _pt_vmu = pulsar_dreamcast_ble::poll_timing::start();
                    // Hardware-timed PWM/EasyDMA TX (~1.7ms on the wire, CPU
                    // awaits so the executor keeps running). Fire-and-forget:
                    // no ACK read — a corrupted frame is dropped by the VMU's
                    // CRC and replaced by the next refresh. NOTE: with the
                    // await inside, the poll-timing vmu span is wall time
                    // including whatever other tasks ran, not pure TX cost.
                    host.write_vmu_lcd_dma(&mut bus, &send_buf).await;
                    #[cfg(feature = "poll-timing")]
                    pulsar_dreamcast_ble::poll_timing::record_vmu(_pt_vmu, true);
                    // Opens the shadow the following polls are classified
                    // against — counted after the TX, never around it.
                    #[cfg(feature = "poll-period-debug")]
                    pulsar_dreamcast_ble::poll_period::note_frame();
                    vmu_frame_dirty = false;
                    vmu_polls_since_send = 0;
                    // The frame is on the wire and the bus released. This is the
                    // gap clock's zero — counted after the TX, never around it.
                    #[cfg(feature = "spim-capture")]
                    {
                        let end = Instant::now();
                        read_sched.note_lcd_written(end.as_ticks());
                        lcd_end = Some(end);
                    }
                }
            } else if !read_window {
                fail_count = fail_count.saturating_add(1);
                // Either channel: `maple-fail-debug` and `poll-period-debug`
                // are mutually exclusive (they share report bytes 4-7), so
                // gating this on the former alone left the latter's failure tag
                // reading a counter that never moved.
                #[cfg(any(feature = "maple-fail-debug", feature = "poll-period-debug"))]
                {
                    use core::sync::atomic::Ordering;
                    pulsar_dreamcast_ble::MAPLE_FAIL_TOTAL.fetch_add(1, Ordering::Relaxed);
                    let streak = u8::try_from(fail_count).unwrap_or(u8::MAX);
                    pulsar_dreamcast_ble::MAPLE_FAIL_MAX_CONSEC
                        .fetch_max(streak, Ordering::Relaxed);
                }
                // A poll that didn't answer can't vouch for Start still being
                // held — drop the mirror rather than let it go stale. The next
                // good poll restores it within one cycle.
                pulsar_dreamcast_ble::MAPLE_START_HELD
                    .store(false, core::sync::atomic::Ordering::Relaxed);
                if fail_count == CONTROLLER_LOST_THRESHOLD {
                    log!("MAPLE: Controller lost, re-detecting...");
                    RAW_CONTROLLER_STATE.signal(ControllerState::default());
                    last_state = None;
                    // The controller — and the VMU docked in it — may have
                    // power-cycled rather than merely glitched. Force an
                    // immediate re-probe instead of waiting out the 3s cadence,
                    // and redraw as soon as it answers.
                    vmu_present = false;
                    presence_known = false; // nothing has answered since the loss

                    // tell the host service here, not at the next
                    // window head — there may not be one for minutes. The
                    // re-detection loop below blocks until the controller
                    // answers, the connection drops or the unit sleeps, and a
                    // service still reporting the pad idle and a VMU present
                    // would go on accepting READs that cannot progress.
                    //
                    // `set_vmu_present(false)` ends the generation, which turns
                    // an outstanding read into `DISCARDED` rather than leaving
                    // the dongle to time it out. The pad that was idle is gone
                    // too: the next controller's first sample starts the clock,
                    // and until then "unknown" reads as in use.
                    #[cfg(feature = "spim-capture")]
                    {
                        use pulsar_dreamcast_ble::ble::host_vmu;
                        idle_watch.reset();
                        host_vmu::set_idle(false);
                        host_vmu::set_vmu_present(false);
                        reader.clear();
                    }
                    last_vmu_probe = None; // re-probe on the next pass
                    vmu_frame_dirty = true;
                    // Red, and — once the search has outlasted its grace — the
                    // gauge beside it. This used to be status only, on the view
                    // that bars imply a working link. That was reversed: a
                    // cell sagging under load drops the boost mid-session and
                    // lands exactly here, and with no VMU answering the bars are
                    // the only way to say "charge me" rather than "no controller".
                    status.searching();
                    // SAFETY: `DetectBatteryWatch::start` is unsafe only because it may
                    // enter System Off via `sleep_now` on a critically low reading, which
                    // carries that function's contract: an initialised SoftDevice (enabled
                    // during setup, before this point) and divergence if it fires.
                    let mut detect_battery = unsafe {
                        DetectBatteryWatch::start(&mut battery, &mut power, &mut status).await
                    };

                    let mut retry_delay_ms: u64 = INITIAL_RETRY_DELAY_MS;
                    let redetect_start = Instant::now();
                    loop {
                        // Abort re-detection if BLE disconnects
                        if get_connection_state() != ConnectionState::Connected {
                            break;
                        }

                        if board::SUPPORTS_SLEEP
                            && redetect_start.elapsed().as_millis() >= SLEEP_TIMEOUT_MS
                        {
                            log!("MAPLE: Re-detect timeout, entering System Off");
                            // SAFETY: `sleep_now` requires an initialised SoftDevice and never returns.
                            // Both hold here: the SoftDevice is enabled during setup, well before this
                            // point, and this call diverges — nothing after it can observe the
                            // torn-down pin state.
                            unsafe {
                                sleep_now(&mut power, &mut status);
                            }
                        }

                        // The sleep hold. Nothing is draining here: losing the
                        // controller ended the generation above
                        // (`set_vmu_present(false)`), and every staged block went
                        // back to the host DISCARDED. So the 7 s hold sleeps at
                        // once, like the 15 s.
                        if goodbye_due() {
                            log!("MAIN: re-detect goodbye — entering sleep");
                            // SAFETY: as at the re-detect timeout above — an
                            // initialised SoftDevice, and this call diverges.
                            unsafe {
                                sleep_now(&mut power, &mut status);
                            }
                        }

                        // Keep re-asserting the IP5306 here too. This loop blocks
                        // for up to `SLEEP_TIMEOUT_MS`, and the periodic refresh
                        // sits *after* it — so if cleared enable bits are why the
                        // controller went quiet, the firmware would wait for a
                        // controller that cannot answer before repairing the
                        // configuration that would let it.
                        if last_ip5306_refresh.elapsed() >= IP5306_REFRESH_INTERVAL {
                            refresh_power_config(&mut power).await;
                            last_ip5306_refresh = Instant::now();
                        }

                        // SAFETY: as at `DetectBatteryWatch::start` above — may enter
                        // System Off via `sleep_now`, whose contract (an initialised
                        // SoftDevice, divergence if it fires) holds here.
                        unsafe {
                            detect_battery
                                .tick(&mut battery, &mut power, &mut status)
                                .await;
                        }

                        let result = host.request_device_info(&mut bus);
                        if let MapleResult::Ok(_) = &result {
                            log!("MAPLE: Controller re-detected");
                            status.connected();
                            // The search has been reading the battery; normal
                            // play shows what it found rather than what Phase 3
                            // last knew. The frame is already marked dirty.
                            let snap = battery.snapshot();
                            vmu_battery_percent = snap.shown_percent(vmu_usb_present);
                            vmu_battery_charging = snap.charging == Some(true);
                            fail_count = 0;
                            last_activity = Instant::now();
                            break;
                        }
                        Timer::after(Duration::from_millis(retry_delay_ms)).await;
                        retry_delay_ms = (retry_delay_ms * 2).min(MAX_RETRY_DELAY_MS);
                    }

                    // If BLE disconnected during re-detection, break to outer loop
                    if get_connection_state() != ConnectionState::Connected {
                        log!("MAIN: BLE disconnected during controller re-detect");
                        // The generation already ended with the controller
                        // (`set_vmu_present(false)` above); this is the
                        // leaving-the-loop rule applied for uniformity.
                        pulsar_dreamcast_ble::ble::host_vmu::unwatched();
                        // As at the poll loop's own disconnect: the rail is held
                        // through the grace, so the motor has to be stopped here.
                        rumble.set(0);
                        pulsar_dreamcast_ble::RUMBLE_LEVEL.reset();
                        // From the drop: a drain hold may have kept this loop
                        // running since `link_lost_at`, as at the leave path.
                        rail_hold = RailHold::after_drop(link_lost_at.unwrap_or_else(Instant::now));
                        status.off();
                        RAW_CONTROLLER_STATE.signal(ControllerState::default());
                        pulsar_dreamcast_ble::MAPLE_START_HELD
                            .store(false, core::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
            }

            // Re-assert the IP5306 configuration periodically. `SYS_CTL0` is
            // written at connect (`rail_on`) and otherwise never verified; a VIN transition
            // (unplugging USB) is exactly the kind of event that can perturb the
            // chip, and if the boost bit comes back clear the 5 V rail stays
            // down with nothing else able to restore it — which is why the board
            // could never settle back to a steady state.
            //
            // 10s, not the 60s battery cadence: the IP5306's light-load dwell is
            // as short as 8s (`SYS_CTL2[3:2]`), so a 60s refresh could miss the
            // window entirely. One I2C read, and a write only on real drift.
            if last_ip5306_refresh.elapsed() >= IP5306_REFRESH_INTERVAL {
                refresh_power_config(&mut power).await;
                last_ip5306_refresh = Instant::now();
            }

            {
                // Monitor USB state changes — toggle boost accordingly. Compiled
                // out on boards without a passthrough rail to hand off to.
                if board::HAS_USB_PASSTHROUGH {
                    let usb_now = power.is_externally_powered();
                    if usb_now != usb_powered {
                        usb_powered = usb_now;
                        if usb_now {
                            log!("PWR: USB connected, disabling boost (passthrough)");
                            power.rail_off();
                        } else {
                            log!("PWR: USB removed, enabling boost");
                            power.rail_on();
                        }
                    }
                }

                let charging = power.is_charging();
                if charging != was_charging {
                    log!(
                        "CHG: {}",
                        if charging {
                            "Charging started"
                        } else {
                            "Charging stopped"
                        }
                    );
                    was_charging = charging;
                }

                if battery.due(battery_policy::Context::Normal) {
                    // SAFETY: `BatteryMonitor::sample` is unsafe only because it may enter
                    // System Off via `sleep_now` on a critically low reading, which carries
                    // that function's contract: an initialised SoftDevice (enabled during
                    // setup, before this point) and divergence if it fires.
                    unsafe {
                        battery
                            .sample(&mut power, &mut status, battery_policy::Context::Normal)
                            .await;
                    }
                }

                // Presentation is evaluated every pass, not only when a reading
                // is due. The hardware read stays on its cadence, but a fact
                // expires on the clock: with the last good reading followed by
                // failed ones, waiting for the next attempt kept an expired
                // level on screen for up to a further interval, while the detect
                // loops — which do check every pass — had already hidden it.
                // Cheap: a snapshot is two comparisons,
                // and both displays write only on a change.
                let snap = battery.snapshot();
                // Redraw on a *change* only. Nothing else marks the frame dirty
                // on a battery read, so without this the bolt would not appear
                // until some unrelated update happened to dirty the frame — a
                // large part of why plugging in looked inert. `shown_percent`
                // takes the VBUS poll above, at most a second old, so the 100
                // appears and disappears with the cable, not with the 60 s read.
                let shown = snap.shown_percent(vmu_usb_present);
                let charging = snap.charging == Some(true);
                if shown != vmu_battery_percent || charging != vmu_battery_charging {
                    vmu_battery_percent = shown;
                    vmu_battery_charging = charging;
                    vmu_frame_dirty = true;
                }
                // Same value feeds the VMU icon and the WS2812 gauge, and both
                // bucket it through `vmu::bars_for_percent`, so the two displays
                // cannot disagree. Suppressed while a VMU is docked — it already
                // shows this. `set_battery` no-ops when nothing changed, so no
                // redundant DMA per pass.
                //
                // Gated on `presence_known`: until the first device-info reply
                // decodes, `vmu_present == false` means "not asked yet", not "no
                // VMU". Rendering it flashed the bars on a docked board. Leaving
                // the gauge untouched is right in both directions — a docked VMU
                // is already showing the level, and an empty bay lights the bars
                // one probe later.
                if presence_known {
                    status.set_battery(if vmu_present {
                        None
                    } else {
                        vmu_battery_percent
                    });
                }
            }

            // Not while acked saves are draining onto the VMU: the sleep loses
            // them. The drain is bounded (`LINK_LOSS_DRAIN_DEADLINE_MS` after a
            // drop; the dongle's own queue while connected), so this is a
            // deferral, not a hole.
            if board::SUPPORTS_SLEEP
                && last_activity.elapsed().as_millis() >= INACTIVITY_TIMEOUT_MS
                && !pulsar_dreamcast_ble::ble::host_vmu::draining()
            {
                log!("MAIN: Inactivity timeout (10 min), entering System Off");
                // SAFETY: `sleep_now` requires an initialised SoftDevice and never returns.
                // Both hold here: the SoftDevice is enabled during setup, well before this
                // point, and this call diverges — nothing after it can observe the
                // torn-down pin state.
                unsafe {
                    sleep_now(&mut power, &mut status);
                }
            }

            #[cfg(feature = "poll-timing")]
            pulsar_dreamcast_ble::poll_timing::tick_and_log();

            // On-device collision/bad-roll detector: a body past the budget
            // means this iteration's Maple transactions collided (retries)
            // or something new is slow. Edge-alignment waits (align_extra)
            // are honest scheduling, not body work — excluded.
            if iter_start.elapsed() >= Duration::from_millis(BODY_BUDGET_MS) + align_extra {
                pulsar_dreamcast_ble::POLL_OVERRUNS
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }

            // spend what is left of the window on the decoder. A
            // block takes ~25 ms of slices across several windows, and the
            // explicit yield is not optional — this is CPU-bound work and
            // Embassy will not preempt it, so without one the BLE notify and the
            // SoftDevice runner would not run between slices.
            #[cfg(feature = "spim-capture")]
            {
                let deadline = iter_start + Duration::from_millis(DECODE_UNTIL_MS);
                let guard = Duration::from_micros(SLICE_GUARD_US);
                while reader.decoding() && Instant::now() + guard < deadline {
                    reader.run_slice();
                    embassy_futures::yield_now().await;
                }
            }

            // Radio-aware pacer (see the POLL_PERIOD_MS docs): sleep the
            // minimum spacing, then start the next iteration only at the
            // head of a radio-quiet window — a fresh INACTIVE edge, bounded
            // so an absent or late notification slows one iteration at
            // most; with notifications unavailable (`None`), this degrades
            // to exactly the fixed-cadence regime. The minimum-spacing
            // sleep always returns Pending at least once, so the executor's
            // other tasks run every iteration.
            #[cfg(feature = "poll-period-debug")]
            let _pp_sleep = pulsar_dreamcast_ble::poll_period::stamp_wall();
            Timer::at(iter_start + Duration::from_millis(MIN_POLL_SPACING_MS)).await;
            let _ = align_to_quiet_window(POLL_ALIGN_CAP_MS).await;
            #[cfg(feature = "poll-period-debug")]
            pulsar_dreamcast_ble::poll_period::record_sleep(_pp_sleep);
        }
    }
}

/// `SoftDevice` runner task - must run continuously.
/// Logs SOC events: `POFWARN` here plus a wall-clock correlation with VMU
/// writes is the test of the rail-dip theory for the SD asserts.
#[embassy_executor::task]
async fn softdevice_task(sd: &'static Softdevice) {
    sd.run_with_callback(|evt| match evt {
        nrf_softdevice::SocEvent::PowerFailureWarning => {
            log!("PWR: POFWARN — supply dipped below 2.5V");
        }
        // `log!` compiles to nothing without `rtt`, so `other` reads as unused
        // in production builds — it's only referenced for diagnostics.
        // `_other` rather than `other`: `log!` compiles to nothing without `rtt`,
        // so the binding is genuinely unused in production builds and used in
        // instrumented ones. An #[expect] would be correct in only one of them.
        _other => log!("SOC: event {}", _other as u32),
    })
    .await;
}
