// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Bluetooth LE adapter for Dreamcast controllers.
//!
//! Speaks the Dreamcast Maple Bus protocol over GPIO and presents controller
//! input as an Xbox One S BLE HID gamepad. Built on Embassy async with the
//! Nordic `SoftDevice` S140 BLE stack.

#![no_std]

use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, ThreadModeRawMutex};
use embassy_sync::signal::Signal;

/// RTT print macro — compiles to nothing when the `rtt` feature is disabled.
///
/// Formats into a stack buffer FIRST, then hands the finished string to RTT:
/// `rprintln!` runs the entire `format_args` rendering inside rtt-target's
/// critical section, and a ~120-char POLLTIME line with ten integer
/// conversions is a 50-200µs interrupt blackout. At 2 log lines/sec that
/// produced stochastic SoftDevice assertion panics (~1/min) in every
/// instrumented build of 2026-06-10/11 — see `poll_timing`'s module docs for
/// the full post-mortem. With pre-formatting, the critical section shrinks
/// to a memcpy of the rendered bytes. Lines over 256 chars are truncated.
#[cfg(feature = "rtt")]
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let mut _s: heapless::String<256> = heapless::String::new();
        let _ = core::write!(_s, $($arg)*);
        rtt_target::rprintln!("{}", _s.as_str());
    }};
}

/// RTT print macro — compiles to nothing when the `rtt` feature is disabled.
#[cfg(not(feature = "rtt"))]
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{}};
}

/// RTT init — compiles to nothing when the `rtt` feature is disabled.
#[cfg(feature = "rtt")]
#[macro_export]
macro_rules! log_init {
    () => {
        rtt_target::rtt_init_print!()
    };
}

/// RTT init — compiles to nothing when the `rtt` feature is disabled.
#[cfg(not(feature = "rtt"))]
#[macro_export]
macro_rules! log_init {
    () => {{}};
}

pub mod ble;
pub mod board;
pub mod button;
pub mod maple;
pub mod panic_handler;
#[cfg(feature = "poll-period-debug")]
pub mod poll_period;
#[cfg(feature = "poll-timing")]
pub mod poll_timing;
pub mod vmu;

/// Count of poll-loop body-budget overruns: iterations whose body (poll top
/// to pacer, excluding all pacer waiting) ran past `BODY_BUDGET_MS`.
///
/// With
/// the radio-quiet gate active a body only exceeds the budget when its Maple
/// transaction collided anyway (retries) or something new got slow — so this
/// is the always-compiled production self-check for a bad roll or a gate
/// regression (see the 2026-08-05 board bring-up measurements: on
/// the pre-gate builds, collision-dwelling rolls announced themselves here
/// at ~37/s). Read out over the HID side channel by the `poll-period-debug`
/// feature (tag `0xB5`).
pub static POLL_OVERRUNS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// BLE HID notification interval (~125Hz, matches Xbox One S).
pub const NOTIFY_INTERVAL_MS: u64 = 8;

/// Delay before sending the first HID notify, giving the host time to
/// finish service discovery and write the CCCD that subscribes to
/// notifications.
///
/// Reports sent before subscription return an error from
/// `report_notify` and count toward `MAX_NOTIFY_FAILURES` — too short and
/// we'll disconnect a slow-subscribing host.
///
/// Original value 5000 ms (commit 1d66d2c) bundled pairing time too, but
/// pairing is now handled separately in `handle_connection` (~600 ms of
/// explicit waits) before the notify task starts, so this only needs to
/// cover service discovery + CCCD write, which macOS typically completes
/// in well under 1 s. 1500 ms keeps a comfortable margin.
pub const SERVICE_DISCOVERY_DELAY_MS: u64 = 1500;

/// Max consecutive BLE notify failures before disconnecting.
pub const MAX_NOTIFY_FAILURES: u8 = 10;

/// Timeout before entering sleep when disconnected (ms).
pub const SLEEP_TIMEOUT_MS: u64 = 60_000;

/// Latest raw Dreamcast controller sample shared between thread-mode tasks.
///
/// The Maple producer publishes every successful poll, including small analog
/// movements that [`maple::ControllerState::state_changed`] intentionally
/// filters out for inactivity/HID wake purposes. The active BLE personality is
/// the single consumer: normal HID during ordinary boots, or `LiveInput` during
/// an exclusive configuration boot.
///
/// Both sides run on Embassy's one thread-mode executor and neither accesses
/// this slot from an interrupt. `ThreadModeRawMutex` therefore gives a
/// whole-value latest-sample handoff without masking SoftDevice interrupts on
/// every ~66 Hz Maple poll. It also asserts if that executor-only invariant is
/// accidentally broken later.
pub static RAW_CONTROLLER_STATE: Signal<ThreadModeRawMutex, maple::ControllerState> = Signal::new();

/// Signal to trigger sync/pairing mode (clears bonds).
pub static SYNC_MODE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Signal to switch active BLE profile and reset. Carries the new `ProfileId`.
pub static PROFILE_CHANGE: Signal<CriticalSectionRawMutex, ble::ProfileId> = Signal::new();

/// Set by the button task on a 10-second hold to request a graceful System Off.
///
/// The main task picks this up, writes a "BYE" splash to the VMU, briefly
/// holds, then enters System Off. Avoids sleeping mid-write so the goodbye
/// frame actually lands on the LCD.
pub static GOODBYE_PENDING: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Set by the BLE task on the rising edge of the Guide chord (L+R+Start held).
///
/// The main poll loop picks this up and briefly flashes a "home" glyph on the
/// VMU LCD, then resumes normal content. Purely cosmetic and strictly
/// best-effort: a single non-blocking atomic store on the BLE side, and the
/// main loop is free to drop it (goodbye in flight, frame CRC-collision) — it
/// must never perturb Maple bus timing or the controller poll.
pub static GUIDE_GLYPH_PENDING: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Mirror of the Dreamcast controller's Start button.
///
/// Updated by the main task's poll loop on every `Get Condition`
/// (`RAW_CONTROLLER_STATE` can't serve here: it is a consumed latest-value
/// slot).
///
/// Cleared on
/// every failed poll and on leaving the poll loop, so it can never report a
/// stale press. The button task reads it during a sync-button hold to detect
/// the OTA DFU gesture — which therefore only works while a controller is
/// powered and being polled (Phase 3), the only time Start is observable at
/// all.
pub static MAPLE_START_HELD: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Set by the button task when the OTA DFU gesture (sync + Start past 5s)
/// fires.
///
/// The main poll loop consumes it: the reboot must come from the main
/// task, not the button task, because main owns the Maple bus — it can land a
/// "BOOT" splash on the VMU first and reset between polls instead of mid-write.
///
/// Main also owns the battery gauge, so the gate lives there too: below
/// `DFU_MIN_BATTERY_PCT` and not charging, the request is refused with a held
/// "CHRG" splash instead of a reboot.
/// The splash then survives into DFU mode as the "updating" indicator: the LCD
/// holds its last frame while dock power holds, and on pulsarv1 the 5V rail is
/// the IP5306's autonomous boost, which an MCU reset doesn't touch.
pub static DFU_PENDING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// One-boot request for the isolated browser-configuration personality.
///
/// GPREGRET2 bit 2 is deliberately outside both shipped bootloaders' entry
/// protocol: Adafruit does not inspect GPREGRET2, while the Nordic secure
/// bootloader reserves a masked `0xA9` pattern and leaves this low bit for
/// application signalling. This remains a spike hypothesis until it is proved
/// on both physical bootloader paths (Story 001).
const CONFIG_BOOT_MARKER: u32 = 0x04;

/// Consume the one-boot configuration marker before enabling the SoftDevice.
///
/// Call this immediately after `embassy_nrf::init`: that function may reset
/// once while provisioning APPROTECT, and the marker must survive that reset.
/// The SoftDevice is still disabled at this point, so direct POWER access is
/// permitted. Only our bit is cleared; bootloader-owned bits are preserved.
#[must_use]
pub fn take_config_boot_marker() -> bool {
    // nRF52840 POWER base 0x4000_0000 + GPREGRET2 offset 0x520.
    // Embassy keeps its PAC private unless the unstable-pac feature is enabled;
    // this single pinned register is clearer than widening that dependency API.
    const GPREGRET2: *mut u32 = 0x4000_0520 as *mut u32;

    // SAFETY: GPREGRET2 is a 32-bit aligned nRF52840 MMIO register. This runs
    // after Embassy init but before SoftDevice enable, so the application owns
    // POWER access. No other task exists yet and the pointer is not retained.
    let retained = unsafe { core::ptr::read_volatile(GPREGRET2) };
    let requested = (retained & CONFIG_BOOT_MARKER) != 0;
    if requested {
        // SAFETY: same exclusive pre-SoftDevice MMIO access as the read above.
        // Read-modify-write clears only the application-owned marker bit.
        unsafe { core::ptr::write_volatile(GPREGRET2, retained & !CONFIG_BOOT_MARKER) };
        cortex_m::asm::dsb();
    }
    requested
}

/// Set the one-boot configuration marker and reset if the write is verified.
///
/// Returns `false` without resetting if the SoftDevice rejects the write or a
/// readback does not contain the marker. The caller can then suppress the
/// normal pairing action for this hold instead of silently clearing the bond.
#[must_use]
pub fn reboot_into_config() -> bool {
    use nrf_softdevice_s140 as sd_raw;

    // SAFETY: these are SoftDevice SVCs operating on GPREGRET2 (register id 1).
    // The first takes only integer values; the second writes one `u32` owned on
    // this stack and does not retain its pointer. The SoftDevice has been
    // enabled before the button task can call this function.
    let set_rc = unsafe { sd_raw::sd_power_gpregret_set(1, CONFIG_BOOT_MARKER) };
    let mut retained: u32 = 0;
    // SAFETY: same enabled-SoftDevice precondition as above; `retained` is a
    // valid, uniquely borrowed output word for the duration of the SVC.
    let get_rc = unsafe { sd_raw::sd_power_gpregret_get(1, &raw mut retained) };
    if set_rc != 0 || get_rc != 0 || (retained & CONFIG_BOOT_MARKER) == 0 {
        return false;
    }

    cortex_m::peripheral::SCB::sys_reset();
}

/// Bootloader OTA-entry magic, board-scoped to match the bootloader each
/// board actually carries (ADR-014):
/// - pulsarv1 (retail): Nordic Secure DFU `BOOTLOADER_DFU_START` (`0xB1`) —
///   the bootloader checks bit 0 of `GPREGRET`, `0xB1` is the documented
///   buttonless-entry value.
/// - XIAO dev boards: Adafruit `DFU_MAGIC_OTA_RESET` (`0xA8`).
#[cfg(feature = "board-pulsarv1")]
const DFU_MAGIC_OTA_RESET: u32 = 0xB1;
#[cfg(not(feature = "board-pulsarv1"))]
const DFU_MAGIC_OTA_RESET: u32 = 0xA8;

/// Whether USB VBUS is present — a charger or host is plugged in.
///
/// Read through the SoftDevice SVC rather than the raw `0x4000_0438` register
/// that `board::xiao::is_usb_connected` uses: POWER is a restricted peripheral
/// while the SoftDevice is enabled, and `sd_power_usbregstatus_get` is the
/// sanctioned accessor. (The xiao path predates this and is worth converting.)
///
/// This is the honest "power is going in" signal on pulsarv1, and deliberately
/// preferred over the IP5306's gauge bits for display purposes: `charging`
/// (`0x70` bit 3) goes false the instant the pack tops off, so a full battery on
/// a charger reads exactly like one on no charger, and `full`'s behaviour once
/// unplugged has never been verified on hardware. VBUS is a hardware line — on
/// this carrier the charge input *is* the XIAO's own USB-C (`VBUS → D2 → IP5306
/// VIN`), so its presence means the cell is being fed.
#[must_use]
pub fn usb_vbus_present() -> bool {
    use nrf_softdevice_s140 as sd_raw;

    let mut status: u32 = 0;
    // SAFETY: a SoftDevice SVC taking a pointer to a `u32` we own on the stack.
    // It writes at most that single word and does not retain the pointer. Its
    // only precondition is an enabled SoftDevice, which holds from
    // `init_softdevice` onward for the life of the program.
    let ret = unsafe { sd_raw::sd_power_usbregstatus_get(&raw mut status) };

    // Bit 0 = VBUSDETECT. On a failed call `status` is untouched, so report
    // "absent" rather than guessing — a bad read must not latch the charge
    // indicator on with no way to clear it.
    ret == 0 && (status & 1) != 0
}

/// Reboot into the board's BLE OTA DFU mode.
///
/// Writes the OTA magic into `GPREGRET` and resets. `GPREGRET` is owned by the
/// SoftDevice while it is enabled, so the write must go through
/// `sd_power_gpregret_*` — a direct register write would be rejected. The
/// precondition is only that the SoftDevice is *enabled*, which holds from
/// `init_softdevice` onward for the life of the program; an active *connection*
/// is not required, and callers no longer imply one — the tap-tap-hold DFU chord
/// is reachable from Phase 1 with nothing connected at all.
///
/// pulsarv1 comes up advertising "PulsarDFU" (secure DFU, `0xFE59`); XIAO dev
/// boards come up as Adafruit's "AdaDFU" (legacy DFU). The DK is flashed bare
/// over J-Link (no bootloader), so there this is just an app reboot —
/// harmless, and it keeps the gesture testable end-to-end up to the reset.
pub fn reboot_into_ota_dfu() -> ! {
    use nrf_softdevice_s140 as sd_raw;

    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "clear-then-set is one indivisible register update; splitting it \
                  would imply the intermediate state is meaningful, and it is not"
    )]
    // SAFETY: both are SoftDevice SVC calls taking a register index and a mask
    // by value — neither dereferences a pointer nor aliases memory. Their only
    // precondition is that the SoftDevice is enabled, which every caller
    // satisfies: `init_softdevice` runs during setup and the SoftDevice is never
    // disabled afterwards, so enablement holds regardless of connection state
    // (see the doc comment above). The clear must precede the set because
    // `gpregret_set` ORs its argument into the register.
    unsafe {
        // A stale configuration request must never be composed with a DFU
        // request. Clear only the application-owned bit in GPREGRET2.
        let _ = sd_raw::sd_power_gpregret_clr(1, CONFIG_BOOT_MARKER);
        let _ = sd_raw::sd_power_gpregret_clr(0, 0xFF);
        let _ = sd_raw::sd_power_gpregret_set(0, DFU_MAGIC_OTA_RESET);
    }

    cortex_m::peripheral::SCB::sys_reset();
}

/// Installed application version, read from the Nordic Secure DFU bootloader
/// settings page. `None` when no plausible settings page is present (UF2
/// boards, bare-J-Link DK, blank flash).
///
/// The settings page is the bootloader's own record of what it installed —
/// the same field its downgrade protection checks — so this is ground truth
/// for "which *package* is on this unit", independent of the app binary
/// itself. That distinction matters: the test ladder re-signs old binaries
/// under new version numbers (see `dist/pulsar_ota_v2*binary.zip`), and the
/// boot-splash tag built from this is what tells those flashes apart.
#[must_use]
pub fn installed_app_version() -> Option<u32> {
    /// Bootloader settings page on the nRF52840 (see `memory.x`:
    /// "0xFF000 bootloader settings").
    const SETTINGS_ADDR: u32 = 0x000F_F000;
    /// `nrf_dfu_settings_t` begins `crc, settings_version, app_version, ...`.
    const SETTINGS_VERSION_OFFSET: u32 = 4;
    const APP_VERSION_OFFSET: u32 = 8;

    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "two reads of the same settings page, sound for the same reason"
    )]
    // SAFETY: fixed, word-aligned addresses inside the bootloader settings
    // flash page, always mapped and readable on this part. Reading flash has
    // no side effects and races nothing: the page is only ever rewritten by
    // the bootloader, which never runs concurrently with the application.
    let (settings_version, app_version) = unsafe {
        (
            core::ptr::read_volatile((SETTINGS_ADDR + SETTINGS_VERSION_OFFSET) as *const u32),
            core::ptr::read_volatile((SETTINGS_ADDR + APP_VERSION_OFFSET) as *const u32),
        )
    };

    // Plausibility gates rather than a CRC walk: layouts 1 and 2 are the only
    // ones this bootloader family writes, and an erased page reads 0xFFFF_FFFF.
    // UF2 boards and the bare DK fail these and report None.
    if (1..=2).contains(&settings_version) && app_version != 0 && app_version != 0xFFFF_FFFF {
        Some(app_version)
    } else {
        None
    }
}

/// Set by the BLE task when one of its disconnected-state timeouts should end
/// in System Off (reconnect timeout, sync timeout with no bond).
///
/// The BLE task can't reach the board's `Power` handle, so calling
/// `board::enter_sleep()` from there skipped `Power::prepare_for_sleep()` — and
/// on pulsarv1 that leaves the IP5306 5 V boost **on** through the whole sleep,
/// so the attached controller drains the LiPo while the adapter is "off". That
/// is the common "walk away from it" path. The main task owns `Power`, so it
/// picks this flag up from its disconnected wait loop and routes through the
/// single `sleep_now()` choke point instead. See [`request_sleep`].
pub static SLEEP_REQUEST: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Ask the main task to power the board down, and never come back.
///
/// Sets [`SLEEP_REQUEST`] and then parks forever: main enters System Off within
/// one tick of its disconnected wait loop (`BLE_WAIT_CHECK_MS`), and the caller
/// must not resume its state machine in that window — returning here would let
/// the BLE task start advertising again on a board that is shutting down.
///
/// Callers must already have checked `board::SUPPORTS_SLEEP`; on boards that
/// can't sleep, nothing would ever consume the flag and this would hang.
pub async fn request_sleep() -> ! {
    SLEEP_REQUEST.store(true, core::sync::atomic::Ordering::Relaxed);
    match core::future::pending::<core::convert::Infallible>().await {}
}

/// Signaled by the button task on any short sync-button press.
///
/// The BLE task
/// uses this as the explicit "wake from silent reconnect-wait" trigger,
/// matching how Xbox / PlayStation controllers use their dedicated wake
/// buttons. Without this signal, the BLE task stays silent after the initial
/// reconnect window so a sleeping host isn't woken by ongoing advertising.
pub static WAKE_REQUEST: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Battery level percentage (0-100) for BLE reporting.
///
/// Signals 0xFF when charging (tells BLE task to report "charging" state).
/// Board-agnostic: boards without a gauge simply never signal it (the BLE
/// battery reader that waits on it is gated separately in `ble::task`).
pub static BATTERY_LEVEL: Signal<CriticalSectionRawMutex, u8> = Signal::new();

/// Debug-only: the latest raw IP5306 gauge sample, packed for smuggling out in
/// the HID report's unused right-stick bytes (`gauge-debug` feature).
///
/// Layout, low byte first: `[raw 0x78, decoded %, flags, MAGIC]`, where flags
/// bit 0 = charging and bit 1 = charge-complete. The magic byte lets a host
/// capture prove it is looking at a `gauge-debug` build rather than a genuine
/// centered right stick.
///
/// This exists because pulsarv1 **cannot be observed over RTT** — the XIAO
/// module has no onboard debugger and there is no SWD probe for it — and the
/// gauge only misbehaves untethered on battery, so a wired channel would change
/// the very condition under test. The HID report is the one link that survives.
#[cfg(feature = "gauge-debug")]
pub static GAUGE_SAMPLE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Sentinel in the high byte of [`GAUGE_SAMPLE`], so a capture can distinguish
/// a real gauge sample from an untouched (centered) right stick.
#[cfg(feature = "gauge-debug")]
pub const GAUGE_MAGIC: u8 = 0xA5;

/// Publish a gauge sample for the `gauge-debug` HID channel. No-op cost in
/// normal builds — the whole thing compiles out.
#[cfg(feature = "gauge-debug")]
pub fn publish_gauge_sample(raw: u8, percent: u8, charging: bool, full: bool) {
    let flags = u8::from(charging) | (u8::from(full) << 1);
    let packed = u32::from(raw)
        | (u32::from(percent) << 8)
        | (u32::from(flags) << 16)
        | (u32::from(GAUGE_MAGIC) << 24);
    GAUGE_SAMPLE.store(packed, core::sync::atomic::Ordering::Relaxed);
}

/// Running count of failed `get_condition` polls (wrapping u16), for the
/// `maple-fail-debug` HID channel. Same rationale as [`GAUGE_SAMPLE`]:
/// pulsarv1 has no RTT path, so the HID report is the only telemetry link.
#[cfg(feature = "maple-fail-debug")]
pub static MAPLE_FAIL_TOTAL: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(0);

/// Longest consecutive `get_condition` failure streak seen since boot
/// (saturated to u8). One failed poll = 5 ms of unsampled input; 3+ in a row
/// is a whole BLE connection interval gone dark.
#[cfg(feature = "maple-fail-debug")]
pub static MAPLE_FAIL_MAX_CONSEC: core::sync::atomic::AtomicU8 =
    core::sync::atomic::AtomicU8::new(0);

/// Sentinel byte marking a `maple-fail-debug` payload in report byte 7.
#[cfg(feature = "maple-fail-debug")]
pub const MAPLE_FAIL_MAGIC: u8 = 0x5A;

#[cfg(all(feature = "gauge-debug", feature = "maple-fail-debug"))]
compile_error!(
    "gauge-debug and maple-fail-debug both overwrite HID report bytes 4-7; enable only one"
);

#[cfg(all(feature = "connparam-debug", feature = "maple-fail-debug"))]
compile_error!(
    "connparam-debug and maple-fail-debug both overwrite HID report bytes 4-7; enable only one"
);

#[cfg(all(feature = "gauge-debug", feature = "connparam-debug"))]
compile_error!(
    "gauge-debug and connparam-debug both overwrite HID report bytes 4-7; enable only one"
);

#[cfg(all(feature = "poll-period-debug", feature = "gauge-debug"))]
compile_error!(
    "poll-period-debug and gauge-debug both overwrite HID report bytes 4-7; enable only one"
);

#[cfg(all(feature = "poll-period-debug", feature = "connparam-debug"))]
compile_error!(
    "poll-period-debug and connparam-debug both overwrite HID report bytes 4-7; enable only one"
);

#[cfg(all(feature = "poll-period-debug", feature = "maple-fail-debug"))]
compile_error!(
    "poll-period-debug and maple-fail-debug both overwrite HID report bytes 4-7; enable only one"
);

/// Debug-only: raw return code of the last `sd_ble_gap_conn_param_update` call
/// (`connparam-debug` feature). `u32::MAX` until an attempt is made.
///
/// Exists because three rounds of connection-parameter tuning (2026-07-27) all
/// produced the same 15 ms interval, and **we cannot tell why**: "host declined"
/// and "the request was never issued" look identical from the host side. The
/// SoftDevice returns `NRF_ERROR_BUSY` (17) if another procedure is in flight,
/// and the request fires 500 ms after `request_security()` while bonding
/// plausibly still is. `log!` is invisible on pulsarv1 (no SWD probe), so the
/// return code has never once been read.
#[cfg(feature = "connparam-debug")]
pub static CONNPARAM_RC: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(u32::MAX);

/// Sentinel in the high byte of the `connparam-debug` report bytes, so a capture
/// can prove it is looking at this build and not a centered right stick.
#[cfg(feature = "connparam-debug")]
pub const CONNPARAM_MAGIC: u8 = 0xC5;

/// Record the outcome of a connection-parameter update request.
#[cfg(feature = "connparam-debug")]
pub fn publish_connparam_rc(rc: u32) {
    CONNPARAM_RC.store(rc, core::sync::atomic::Ordering::Relaxed);
}

/// Rumble intensity (0-255) requested by the BLE host via the HID rumble Output
/// report. The board's `Rumble` handle drives the motor from this; boards
/// without a motor (dk/xiao) ignore it.
pub static RUMBLE_LEVEL: Signal<CriticalSectionRawMutex, u8> = Signal::new();
