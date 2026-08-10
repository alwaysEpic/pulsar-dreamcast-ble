// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use pulsar_dreamcast_ble::ble::{get_connection_state, ConnectionState};
use pulsar_dreamcast_ble::maple::host::MapleResult;
use pulsar_dreamcast_ble::maple::{ControllerState, MapleBus, MapleHost};
use pulsar_dreamcast_ble::{ble, board, CONTROLLER_STATE};

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
/// iteration ran get_condition + sub_peripheral_mask + enumerate_vmu
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

/// Initial retry delay for controller detection (ms).
const INITIAL_RETRY_DELAY_MS: u64 = 100;

/// Maximum retry delay for controller detection (ms).
const MAX_RETRY_DELAY_MS: u64 = 1000;

/// How often to check BLE connection state while waiting (ms).
const BLE_WAIT_CHECK_MS: u64 = 100;

/// Timeout for initial controller detection (ms).
/// Enter System Off if no controller found within 60 seconds of BLE connecting.
const DETECT_TIMEOUT_MS: u64 = 60_000;

/// Timeout before entering sleep when controller is idle (ms).
/// 10 minutes with no input change triggers System Off.
const INACTIVITY_TIMEOUT_MS: u64 = 600_000;

/// Minimum battery percentage to allow an OTA DFU reboot. A unit that dies
/// mid-transfer isn't bricked (the bootloader is never touched), but it
/// strands the user in DFU mode on a draining battery. Charging is always
/// allowed regardless of level — external power is present. 50 is also a
/// clean threshold for pulsarv1's IP5306 gauge, which only reports in 25%
/// steps.
const DFU_MIN_BATTERY_PCT: u8 = 50;

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
/// gauge whose register decode is still ⚠ UNVERIFIED, so the only reading whose
/// meaning is unambiguous under *any* plausible decode is "no LEDs lit" — the
/// gauge itself saying empty. Raise this once an RTT characterization of `0x78`
/// (now logged on every read, `board::pulsarv1::Power::battery`) pins the map
/// down; cutting off at a mis-decoded 25 % would throw away a quarter of the
/// pack's runtime.
const LOW_BATTERY_CUTOFF_PCT: u8 = 0;

/// Consecutive at/below-cutoff readings required before the percent cutoff
/// fires. At `BATTERY_READ_INTERVAL` (60 s) that is ~3 minutes of sustained
/// "empty", so one glitched I²C read can't power down a healthy board — which
/// is the exact failure class that cost the 2026-07-24 debugging session. It
/// also means the boot-time reading alone can never trigger a shutdown.
const LOW_BATTERY_EMPTY_READS: u8 = 3;

/// Sample the battery, publish the level for BLE, and enforce the low-battery
/// cutoffs. Returns the reading so callers can drive the VMU overlay; `None`
/// when the board has no gauge (dk) or the read failed.
///
/// One helper for all three call sites (boot, Phase 1 wait, Phase 3 poll)
/// because they must not drift: the mV guard that fixes pulsarv1's false
/// shutdown only works if *every* site has it.
///
/// Two cutoffs, because the two gauges report different things:
/// - **millivolts** (xiao's SAADC divider): trip below `LOW_BATTERY_CUTOFF_MV`.
/// - **percent** (pulsarv1's IP5306, which reports `millivolts: 0`): trip after
///   `LOW_BATTERY_EMPTY_READS` consecutive readings at or below
///   `LOW_BATTERY_CUTOFF_PCT`.
///
/// Charging batteries and boards that can't sleep are exempt from both.
///
/// # Safety
/// May enter System Off and never return — see [`sleep_now`].
async unsafe fn sample_battery(
    power: &mut board::Power,
    empty_reads: &mut u8,
    status: &mut board::StatusIndicator,
) -> Option<board::BatteryStatus> {
    let bat = power.battery().await?;
    BATTERY_LEVEL.signal(if bat.charging { 0xFF } else { bat.percent });

    if bat.charging || !board::SUPPORTS_SLEEP {
        *empty_reads = 0;
        return Some(bat);
    }

    if bat.millivolts > 0 {
        if bat.millivolts < LOW_BATTERY_CUTOFF_MV {
            log!(
                "PWR: Low battery ({}mV), entering System Off",
                bat.millivolts
            );
            unsafe {
                sleep_now(power, status);
            }
        }
        return Some(bat);
    }

    // Percent-only gauge (pulsarv1's IP5306). `<=`, not `==`: the cutoff is a
    // tunable threshold that merely sits at 0 until the register map is
    // characterized. Clippy reads `u8 <= 0` as always-equality — correct today,
    // wrong the moment the constant is raised, so keep the comparison general.
    #[allow(clippy::absurd_extreme_comparisons)]
    let empty = bat.percent <= LOW_BATTERY_CUTOFF_PCT;
    if empty {
        *empty_reads = empty_reads.saturating_add(1);
        log!(
            "PWR: Gauge empty ({}%), reading {}/{}",
            bat.percent,
            *empty_reads,
            LOW_BATTERY_EMPTY_READS
        );
        if *empty_reads >= LOW_BATTERY_EMPTY_READS {
            log!("PWR: Low battery ({}%), entering System Off", bat.percent);
            unsafe {
                sleep_now(power, status);
            }
        }
    } else {
        *empty_reads = 0;
    }

    Some(bat)
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
    board::enter_sleep()
}

#[allow(clippy::items_after_statements)] // StaticCell pattern requires inline statics
#[embassy_executor::main]
async fn main(spawner: Spawner) {
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

    // Silicon housekeeping: clear bootloader pin residue, then park the onboard
    // QSPI flash in Deep Power Down (no-op on boards that need neither).
    unsafe {
        board::early_init();
    }

    // Load active profile from flash; defaults to Xbox on first boot.
    let profile_id = ble::flash_bond::load_profile();
    let profile = profile_id.profile();
    log!(
        "PROFILE: {} (PID {:#06x})",
        core::str::from_utf8(profile.vmu_label).unwrap_or("?"),
        profile.pid
    );

    // Initialize SoftDevice with chosen profile
    ble::softdevice::set_profile(profile);
    let sd = ble::softdevice::init_softdevice(profile);

    // Power-fail canary for the VMU-write SD-assert investigation: the VMU
    // draws its dock power from the shared 5V rail and its LCD/buzzer
    // activity may dip the supply during writes (debug log 2026-06-11). The
    // power-fail comparator fires a SOC event (logged in softdevice_task)
    // when VDD drops below 2.5V — any POFWARN correlated with a VMU write
    // is direct evidence for the rail-dip theory.
    unsafe {
        use nrf_softdevice_s140 as sd_raw;
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

    // Create HID Gamepad GATT server
    let Ok(server) = ble::GamepadServer::new(sd) else {
        loop {
            cortex_m::asm::wfi();
        }
    };
    static SERVER: StaticCell<ble::GamepadServer> = StaticCell::new();
    let server = SERVER.init(server);
    let _ = server.init(profile);

    // Spawn the SoftDevice runner task
    if let Ok(token) = softdevice_task(sd) {
        spawner.spawn(token);
    }

    // Create bonder for security/pairing
    static BONDER: StaticCell<ble::Bonder> = StaticCell::new();
    let bonder = BONDER.init(ble::Bonder::new());

    // Load bonding data from flash if available
    if let Some((master_id, enc_info, peer_id, sys_attrs)) = ble::flash_bond::load_bond() {
        bonder.load_from_flash(master_id, enc_info, peer_id, sys_attrs);
    }

    // Spawn BLE task
    if let Ok(token) = ble::task::ble_task(sd, server, bonder) {
        spawner.spawn(token);
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
    } = board::init(p);

    if let Ok(token) = pulsar_dreamcast_ble::button::sync_button_task(sync_button, sync_led) {
        spawner.spawn(token);
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

    // Set up Maple Bus using Flex pins
    let mut bus = MapleBus::new(sdcka, sdckb);
    let host = MapleHost::new();

    const BATTERY_READ_INTERVAL: Duration = Duration::from_secs(60);
    let mut last_battery_read: Instant = Instant::now();
    // Consecutive "gauge reads empty" samples, for the percent cutoff. Lives
    // out here so the debounce survives the outer connect/disconnect loop.
    let mut battery_empty_reads: u8 = 0;

    // Initial battery read at startup
    unsafe {
        sample_battery(&mut power, &mut battery_empty_reads, &mut status).await;
    }

    // Outer loop: wait for BLE connection, then poll controller
    loop {
        // --- Phase 1: Wait for BLE connection ---
        log!("MAIN: Waiting for BLE connection...");
        bus.set_low_power();
        status.off();
        loop {
            if get_connection_state() == ConnectionState::Connected {
                break;
            }

            // Goodbye splash from disconnected state. Wake the bus, try to
            // write BYE to the VMU (may silently fail if no controller is
            // plugged in), hold briefly so the user sees it, then sleep.
            // Runs on both boards so the flow is testable on the dev kit;
            // XIAO actually enters System Off, DK halts via WFI.
            if pulsar_dreamcast_ble::GOODBYE_PENDING.load(core::sync::atomic::Ordering::Relaxed) {
                log!("MAIN: Phase 1 goodbye");
                {
                    bus.set_output_mode();
                    Timer::after(Duration::from_millis(20)).await;
                    let mut send_buf = pulsar_dreamcast_ble::vmu::build_message_splash(b"BYE");
                    pulsar_dreamcast_ble::vmu::rotate_180(&mut send_buf);
                    log!("MAIN: VMU enumerate = {}", host.enumerate_vmu(&mut bus));
                    let mut wrote = false;
                    for _ in 0..5 {
                        if host.write_vmu_lcd(&mut bus, &send_buf) {
                            log!("MAIN: BYE write OK");
                            wrote = true;
                            break;
                        }
                        Timer::after(Duration::from_millis(50)).await;
                    }
                    if !wrote {
                        log!("MAIN: BYE write failed (no controller?)");
                    }
                }
                Timer::after(Duration::from_millis(1000)).await;
                log!("MAIN: goodbye — entering sleep");
                unsafe {
                    sleep_now(&mut power, &mut status);
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
                unsafe {
                    sleep_now(&mut power, &mut status);
                }
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

                if last_battery_read.elapsed() >= BATTERY_READ_INTERVAL {
                    unsafe {
                        sample_battery(&mut power, &mut battery_empty_reads, &mut status).await;
                    }
                    last_battery_read = Instant::now();
                }
            }

            Timer::after(Duration::from_millis(BLE_WAIT_CHECK_MS)).await;
        }
        log!("MAIN: BLE connected, enabling controller");

        // --- Phase 2: Enable the controller rail and detect the controller ---
        // Only carriers with a Schottky USB-5V passthrough (xiao) can skip their
        // boost while plugged in. pulsarv1 has no such path — its 5 V comes from
        // the IP5306's autonomous boost, and `is_externally_powered()` there
        // means "charging or topped off", which has nothing to do with who feeds
        // the rail. Gating on the capability keeps the log honest and stops the
        // rail_on/rail_off no-ops from reading like real power decisions.
        let mut usb_powered = board::HAS_USB_PASSTHROUGH && power.is_externally_powered();
        if usb_powered {
            log!("PWR: USB detected, boost off (passthrough)");
        } else {
            power.rail_on();
        }
        // Brief delay for power source startup
        Timer::after(Duration::from_millis(50)).await;

        status.searching();
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
                unsafe {
                    sleep_now(&mut power, &mut status);
                }
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
            power.rail_off();
            continue;
        }

        // --- Phase 3: Poll loop (active gaming) ---
        #[allow(clippy::cast_possible_truncation)]
        let mut vmu_delay: u16 = (3_000 / POLL_PERIOD_MS) as u16; // ~3s before VMU attempt
                                                                  // See the refresh site below. `Instant::now()` only — never subtract a
                                                                  // Duration from it, that panics when the clock is younger than the value.
        const IP5306_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
        let mut last_ip5306_refresh = Instant::now();

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
        const VMU_PROBE_INTERVAL: Duration = Duration::from_secs(5);
        // Consecutive failed probes before believing the VMU is really gone. A
        // probe is a request/response transaction that must survive BLE
        // collisions (~64% of Maple frames collide with a connection event and
        // are dropped), so one failure means nothing. Presence is sticky.
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
        let mut vmu_framebuf =
            pulsar_dreamcast_ble::vmu::build_profile_splash(profile.vmu_glyph, profile.vmu_label);
        let mut vmu_anim_step: u8 = 0;
        let mut vmu_anim_counter: u16 = 0;
        // Splash holds ~30s before transitioning to the pulsar. Derived from
        // the cadence period: with the anchored loop, polls-to-time is finally
        // an honest conversion instead of the old "~17ms per poll" estimate
        // (a bad layout roll used to stretch every poll-counted duration —
        // the lingering boot splash was a visible bad-roll symptom).
        #[allow(clippy::cast_possible_truncation)]
        let mut vmu_splash_polls: u16 = (30_000 / POLL_PERIOD_MS) as u16;
        // Polls to hold the Guide-chord "home" glyph (~1s)
        // before resuming normal content. 0 = not showing it.
        #[allow(clippy::cast_possible_truncation)]
        const VMU_HOME_POLLS: u16 = (1_000 / POLL_PERIOD_MS) as u16;
        let mut vmu_home_polls: u16 = 0;
        // Advance the animation every 20 polls (~260ms, ~4fps). Each frame is
        // a ~1.7ms hardware-timed DMA TX the CPU awaits through — ~0.5ms of
        // average poll period, no bus corruption possible.
        const VMU_ANIM_INTERVAL: u16 = 20;
        let mut vmu_battery_percent: u8 = 100;
        let mut last_state: Option<ControllerState> = None;
        let mut fail_count: u16 = 0;
        let mut last_activity = Instant::now();

        // Goodbye state machine. Activated when GOODBYE_PENDING is set (button
        // task signals at the 7s hold mark, *during* the hold). We render BYE
        // through the existing dirty-flag path so it gets the same radio-idle
        // waiting and retry behavior as the regular pulsar/splash writes.
        // Once the write lands, we hold for ≥1s before triggering System Off
        // (XIAO) or halting via WFI (DK) so the user actually sees BYE.
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
                if pending && goodbye_state.is_none() {
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
                        log!("MAIN: goodbye hold done — entering sleep");
                        unsafe {
                            sleep_now(&mut power, &mut status);
                        }
                    }
                    Some(GoodbyeState::Hold(_)) => {}
                    None => {}
                }
            }

            // Check for BLE disconnect
            let conn_state = get_connection_state();
            if conn_state != ConnectionState::Connected {
                // If the disconnect is because we just entered sync mode, write
                // a SYNC splash to the VMU so it persists through Phase 1.
                if conn_state == ConnectionState::SyncMode {
                    log!("MAIN: Sync mode entered, writing SYNC splash");
                    {
                        let mut send_buf = pulsar_dreamcast_ble::vmu::build_message_splash(b"SYNC");
                        pulsar_dreamcast_ble::vmu::rotate_180(&mut send_buf);
                        // Enumerate for its side effect only: the VMU refuses
                        // BLOCK_WRITE until asked for device info. Its *reply*
                        // never decodes, so this always reports false — the
                        // result must not be assigned to `vmu_present`, which is
                        // what left a dead store here (and its warning).
                        //
                        // Unconditional, matching the Phase 1 BYE splash. It was
                        // guarded on `!vmu_present`, which was harmless only
                        // because presence was permanently false and the
                        // enumerate therefore always ran. Now that presence
                        // works, the guard would skip it and leave the splash
                        // depending on the poll loop having enumerated within the
                        // last 3s. This path runs once, on disconnect, and
                        // already blocks on an LCD write — one more exchange is
                        // not worth the assumption.
                        let _ = host.enumerate_vmu(&mut bus);
                        let _ = host.write_vmu_lcd(&mut bus, &send_buf);
                    }
                }
                log!("MAIN: BLE disconnected, leaving poll loop");
                // Stop the motor before the rail drops, and drop any command the
                // host queued but we never applied. `rail_off` removes the motor
                // supply, so this is not what silences it now — it is what stops a
                // latched duty cycle from resuming the instant the rail comes back
                // on a silent reconnect.
                rumble.set(0);
                pulsar_dreamcast_ble::RUMBLE_LEVEL.reset();
                power.rail_off();
                status.off();
                CONTROLLER_STATE.signal(ControllerState::default());
                pulsar_dreamcast_ble::MAPLE_START_HELD
                    .store(false, core::sync::atomic::Ordering::Relaxed);
                break;
            }

            // Apply any pending rumble command from the host (HID output report).
            if let Some(level) = pulsar_dreamcast_ble::RUMBLE_LEVEL.try_take() {
                rumble.set(level);
            }

            // OTA DFU handoff from the button task. The reset happens here so
            // a BOOT splash can land first, between polls, on a quiet bus. The
            // LCD keeps its last frame while dock power holds, so the splash
            // stays up through DFU mode as the "updating" indicator — on
            // pulsarv1 the 5V rail is the IP5306's autonomous boost, which an
            // MCU reset doesn't touch. (XIAO's discrete boost-enable pin goes
            // hi-Z at reset, so there the rail — and the splash — may drop;
            // retail hardware is pulsarv1.) Deliberately no rail_off here.
            if pulsar_dreamcast_ble::DFU_PENDING.swap(false, core::sync::atomic::Ordering::Relaxed)
            {
                // Battery gate: a fresh read, not the 60s-cadence sample — the
                // charging bit especially must be current. Allowed while
                // charging or at >= DFU_MIN_BATTERY_PCT; a board with no gauge
                // or a failed read (dk) is allowed through, since there is
                // nothing to gate on.
                let too_low = match power.battery().await {
                    Some(bat) if !bat.charging && bat.percent < DFU_MIN_BATTERY_PCT => {
                        Some(bat.percent)
                    }
                    _ => None,
                };

                // `_pct` is log-only: without the rtt feature `log!` compiles
                // to nothing and the binding would otherwise warn as unused.
                if let Some(_pct) = too_low {
                    log!(
                        "MAIN: DFU refused at {}% (need {}% or charger) — showing CHRG",
                        _pct,
                        DFU_MIN_BATTERY_PCT
                    );
                    // Ride the home-glyph hold mechanism: swap WHICH frame the
                    // normal write path sends and let the counter restore the
                    // underlying content afterward — no extra bus traffic. The
                    // flag was consumed by the swap above, so the gesture can
                    // simply be retried (on a charger) after release.
                    vmu_framebuf = pulsar_dreamcast_ble::vmu::build_message_splash(b"CHRG");
                    vmu_frame_dirty = true;
                    vmu_home_polls = VMU_HOME_POLLS * 2; // ~2s
                } else {
                    log!("MAIN: DFU pending — BOOT splash, then OTA bootloader");
                    let mut send_buf = pulsar_dreamcast_ble::vmu::build_message_splash(b"BOOT");
                    pulsar_dreamcast_ble::vmu::rotate_180(&mut send_buf);
                    // Same reasoning as the SYNC splash: enumerate unconditionally
                    // for its side effect (the VMU refuses BLOCK_WRITE until asked
                    // for device info). Both fire-and-forget — a missing or deaf
                    // VMU must not block the reboot.
                    let _ = host.enumerate_vmu(&mut bus);
                    let _ = host.write_vmu_lcd(&mut bus, &send_buf);
                    pulsar_dreamcast_ble::reboot_into_ota_dfu();
                }
            }

            #[cfg(feature = "poll-timing")]
            let _pt_gc = pulsar_dreamcast_ble::poll_timing::start();
            #[cfg(feature = "poll-period-debug")]
            let _pp_gc = pulsar_dreamcast_ble::poll_period::stamp();
            let gc_result = host.get_condition(&mut bus);
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

                let changed = match &last_state {
                    None => true,
                    Some(prev) => prev.state_changed(&state),
                };

                // Only signal on change — avoids overwriting a real button
                // press with identical idle-state data from the next poll.
                if changed {
                    CONTROLLER_STATE.signal(state);
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
                // Best-effort Guide-chord home glyph. Consume the one-shot flag
                // (only when not mid-goodbye) and swap in the house icon; the
                // hold counter keeps it on-screen for ~1s. This only changes
                // WHICH frame the existing single write sends — no extra bus
                // traffic, so Maple timing is untouched. If a chord fires during
                // goodbye it's simply dropped (device is shutting down).
                if !vmu_busy
                    && pulsar_dreamcast_ble::GUIDE_GLYPH_PENDING
                        .swap(false, core::sync::atomic::Ordering::Relaxed)
                {
                    vmu_framebuf = pulsar_dreamcast_ble::vmu::build_home_splash();
                    vmu_frame_dirty = true;
                    vmu_home_polls = VMU_HOME_POLLS;
                }
                if vmu_busy {
                    // Goodbye in flight — leave vmu_framebuf alone.
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
                    // in slot 1). That rides the one RX path proven to decode
                    // — it is how controller detection itself works.
                    //
                    // Addressing the VMU directly at 0x01 never decoded a single
                    // reply on this firmware, so the old `enumerate_vmu`-based
                    // gate was dead from the day it was written.
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
                        // The VMU refuses BLOCK_WRITE until it has been sent a
                        // device-info request, so this request — whose reply has
                        // never decoded — is what keeps the LCD accepting frames.
                        //
                        // Dropping it is what blanked the screen when presence
                        // moved to the controller's reply: the call looked dead
                        // because its result was always false, but the TX was
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
                        vmu_enum_passes = vmu_enum_passes.saturating_add(1);
                        if !was_present || vmu_enum_passes >= 3 {
                            vmu_enum_passes = 0;
                            align_extra += align_to_quiet_window(EXTRA_ALIGN_CAP_MS).await;
                            let _ = host.enumerate_vmu(&mut bus);
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
                        // The VMU shows battery itself, so the LED gauge is only
                        // lit when it can't.
                        status.set_battery(if vmu_present {
                            None
                        } else {
                            Some(vmu_battery_percent)
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
                if vmu_present && vmu_frame_dirty {
                    let mut send_buf = vmu_framebuf;
                    pulsar_dreamcast_ble::vmu::composite_battery(
                        &mut send_buf,
                        vmu_battery_percent,
                        true,
                    );
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
                    vmu_frame_dirty = false;
                }
            } else {
                fail_count = fail_count.saturating_add(1);
                #[cfg(feature = "maple-fail-debug")]
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
                    CONTROLLER_STATE.signal(ControllerState::default());
                    last_state = None;
                    // The controller — and the VMU docked in it — may have
                    // power-cycled rather than merely glitched. Force an
                    // immediate re-probe instead of waiting out the 3s cadence,
                    // and redraw as soon as it answers.
                    vmu_present = false;
                    presence_known = false; // nothing has answered since the loss
                    last_vmu_probe = None; // re-probe on the next pass
                    vmu_frame_dirty = true;
                    // Status only — deliberately no `set_battery` here. A lost
                    // controller is a failure state and red alone says so; adding
                    // the gauge would imply the adapter is working. The bars mean
                    // "here is your battery because the VMU can't show it", which
                    // is a statement about a *working* link.
                    status.searching();

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
                            unsafe {
                                sleep_now(&mut power, &mut status);
                            }
                        }

                        let result = host.request_device_info(&mut bus);
                        if let MapleResult::Ok(_) = &result {
                            log!("MAPLE: Controller re-detected");
                            status.connected();
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
                        power.rail_off();
                        status.off();
                        CONTROLLER_STATE.signal(ControllerState::default());
                        pulsar_dreamcast_ble::MAPLE_START_HELD
                            .store(false, core::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
            }

            // Re-assert the IP5306 configuration periodically. `SYS_CTL0` is
            // written once at boot and never verified; a VIN transition
            // (unplugging USB) is exactly the kind of event that can perturb the
            // chip, and if the boost bit comes back clear the 5 V rail stays
            // down with nothing else able to restore it — which is why the board
            // could never settle back to a steady state.
            //
            // 10s, not the 60s battery cadence: the IP5306's light-load dwell is
            // as short as 8s (`SYS_CTL2[3:2]`), so a 60s refresh could miss the
            // window entirely. One I2C read, and a write only on real drift.
            if last_ip5306_refresh.elapsed() >= IP5306_REFRESH_INTERVAL {
                if power.refresh_config().await {
                    log!("PWR: IP5306 config had drifted — boost/charger re-enabled");
                }
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

                if last_battery_read.elapsed() >= BATTERY_READ_INTERVAL {
                    let bat = unsafe {
                        sample_battery(&mut power, &mut battery_empty_reads, &mut status).await
                    };
                    if let Some(bat) = bat {
                        vmu_battery_percent = if bat.charging { 100 } else { bat.percent };
                        // Same value feeds the VMU icon and the WS2812 gauge, and
                        // both bucket it through `vmu::bars_for_percent`, so the
                        // two displays cannot disagree. Suppressed while a VMU is
                        // docked — it already shows this. `set_battery` no-ops
                        // when nothing changed, so no redundant DMA per read.
                        //
                        // Gated on `presence_known`: until the first device-info
                        // reply decodes, `vmu_present == false` means "not asked
                        // yet", not "no VMU". Rendering it flashed the bars on a
                        // docked board. Leaving the gauge untouched is right in
                        // both directions — a docked VMU is already showing the
                        // level, and an empty bay lights the bars one probe later.
                        if presence_known {
                            status.set_battery(if vmu_present {
                                None
                            } else {
                                Some(vmu_battery_percent)
                            });
                        }
                    }
                    last_battery_read = Instant::now();
                }
            }

            if board::SUPPORTS_SLEEP && last_activity.elapsed().as_millis() >= INACTIVITY_TIMEOUT_MS
            {
                log!("MAIN: Inactivity timeout (10 min), entering System Off");
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
        #[allow(unused_variables)]
        other => log!("SOC: event {}", other as u32),
    })
    .await;
}
