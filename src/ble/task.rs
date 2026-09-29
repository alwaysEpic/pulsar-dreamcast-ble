// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! BLE advertising and connection handling task.

#![expect(
    clippy::too_many_lines,
    reason = "`ble_task` is a flat connect/disconnect/event dispatch that has to be \
              read top to bottom. The attribute is module-scoped because #[expect] \
              cannot reach the function #[embassy_executor::task] generates."
)]

use embassy_time::{Duration, Instant, Timer};
use nrf_softdevice::ble::gatt_server;
use nrf_softdevice::ble::security::SecurityHandler;
use nrf_softdevice::ble::HciStatus;
use nrf_softdevice::Softdevice;

use crate::ble::security::Observed;
use crate::ble::{
    advertise, get_connection_state, set_connection_state, AdvertiseMode, Bonder, ConnectionState,
    GamepadServer,
};
use crate::maple::ControllerState;
use crate::{PROFILE_CHANGE, RAW_CONTROLLER_STATE, SYNC_MODE, WAKE_REQUEST};
use maple_protocol::guide_chord::GuideChord;
use maple_protocol::notify_budget::{NotifyBudget, Outcome, Verdict};
use maple_protocol::remap::RemapTable;

use crate::BATTERY_LEVEL;

/// BLE advertising and connection handling task.
///
/// State machine:
/// - `Reconnecting` (60s): Try to connect to bonded device only
/// - `Idle`: Continue trying bonded device (not discoverable)
/// - `SyncMode` (60s): Discoverable to all, accepts new pairings
/// - `Connected`: Active connection
#[embassy_executor::task]
pub async fn ble_task(
    sd: &'static Softdevice,
    server: &'static GamepadServer,
    bonder: &'static Bonder,
    remap: RemapTable,
) {
    let mut flash = nrf_softdevice::Flash::take(sd);

    // Sync mode timeout: 60 seconds
    #[expect(
        clippy::items_after_statements,
        reason = "the constant is declared beside the loop that consumes it; hoisting it to module scope would separate a tuning value from the only code it tunes"
    )]
    const SYNC_TIMEOUT_MS: u64 = 60_000;

    // Tracks whether we've completed at least one successful connection during
    // this power session. Boot does an initial reconnect burst; every later
    // disconnect is treated as user-intentional and stays silent until an
    // explicit WAKE_REQUEST or SYNC_MODE arrives. Without this, the Deck's
    // "Disconnect" button would just immediately re-pair, and that's exactly
    // the loop the user complained about.
    let mut had_connection = false;

    loop {
        // Check for profile switch request (non-blocking)
        if PROFILE_CHANGE.signaled() {
            let next = PROFILE_CHANGE.wait().await;
            log!(
                "PROFILE: Switching to {}",
                core::str::from_utf8(next.profile().vmu_label).unwrap_or("?")
            );
            let _ = crate::ble::prefs::save_profile(&mut flash, next).await;
            // Reset to bring up the SoftDevice with the new profile's descriptor.
            cortex_m::peripheral::SCB::sys_reset();
        }

        let state = get_connection_state();

        match state {
            ConnectionState::Reconnecting | ConnectionState::Idle => {
                // Reconnect strategy (matches Xbox / PS controller behavior):
                //  1. One fast-advertising burst (10s, configured in softdevice
                //     advertising config) to catch a brief disconnect or a host
                //     that's still awake.
                //  2. If that times out without a connection, go silent. Don't
                //     keep advertising — it'd repeatedly wake a sleeping host.
                //  3. Wait for an explicit wake signal (sync-button short press
                //     -> WAKE_REQUEST, or 3s hold -> SYNC_MODE).
                //  4. After SLEEP_TIMEOUT_MS total disconnected time, sleep
                //     (XIAO) or fall to Idle (DK).
                let total_start = Instant::now();
                let conn = if bonder.has_bond() {
                    // On boot, do one initial reconnect burst. After any
                    // successful connection in this session, a disconnect is
                    // treated as user-intentional — we go straight to silent
                    // wait until SYNC_MODE or WAKE_REQUEST arrives.
                    let mut wake_pending = !had_connection;
                    loop {
                        if wake_pending {
                            // Phase A: active reconnect window
                            let adv_future =
                                advertise(sd, server, bonder, AdvertiseMode::ReconnectFast);
                            let sync_future = SYNC_MODE.wait();
                            match embassy_futures::select::select(adv_future, sync_future).await {
                                embassy_futures::select::Either::First(Ok(c)) => break Some(c),
                                embassy_futures::select::Either::First(Err(_)) => {
                                    // Phase A timed out without a connection.
                                    // Fall through to the timeout check and
                                    // then Phase B (silent wait). The attempt
                                    // is spent, so clear `wake_pending`: Phase
                                    // B's wake-handler is what re-arms it when
                                    // the user explicitly asks to retry. It
                                    // used to be left set, which was harmless
                                    // only while Phase B could never return
                                    // without a gesture — now that it also
                                    // returns on the sleep deadline, leaving it
                                    // set would buy a second unasked-for burst
                                    // before the unit is allowed to sleep.
                                    log!("BLE: Reconnect window elapsed, going silent");
                                    wake_pending = false;
                                }
                                embassy_futures::select::Either::Second(()) => {
                                    log!("BLE: Sync mode requested");
                                    // The stored bond stays. It is replaced
                                    // only when a new peer actually bonds
                                    // ; an aborted window is a
                                    // no-op for the host we already have.
                                    set_connection_state(ConnectionState::SyncMode);
                                    break None;
                                }
                            }
                        }

                        // Total-disconnect timeout: bail to System Off (XIAO) or
                        // Idle (DK).
                        if total_start.elapsed().as_millis() >= crate::SLEEP_TIMEOUT_MS {
                            if crate::board::SUPPORTS_SLEEP {
                                // Hand off to main rather than sleeping here:
                                // main owns `Power` and powers the 5 V boost
                                // down before System Off. Never returns.
                                log!("BLE: Reconnect timeout, requesting System Off");
                                crate::request_sleep().await;
                            } else {
                                log!("BLE: Reconnect timeout, entering idle");
                                set_connection_state(ConnectionState::Idle);
                                break None;
                            }
                        }

                        // Phase B: silent, waiting for an explicit wake gesture.
                        // Drain any stale WAKE_REQUEST so we don't immediately
                        // re-trigger from a wake that happened during phase A.
                        if WAKE_REQUEST.signaled() {
                            WAKE_REQUEST.wait().await;
                        }

                        // Phase B waits on signals alone, so without a deadline
                        // the total-disconnect check above could never fire: a
                        // unit nobody touches would sit here awake forever
                        // instead of sleeping. Bound the wait by what is left
                        // of the sleep budget and let the loop re-check.
                        let sleep_budget = Duration::from_millis(
                            crate::SLEEP_TIMEOUT_MS
                                .saturating_sub(total_start.elapsed().as_millis()),
                        );
                        let wake_future = WAKE_REQUEST.wait();
                        let sync_future = SYNC_MODE.wait();
                        match embassy_futures::select::select3(
                            wake_future,
                            sync_future,
                            Timer::after(sleep_budget),
                        )
                        .await
                        {
                            embassy_futures::select::Either3::First(()) => {
                                log!("BLE: Wake requested, advertising");
                                wake_pending = true;
                                // The wake spends the host-intentional
                                // disconnect that sent us silent. Left set, the
                                // session this wake produces would inherit it:
                                // its first accidental drop would skip the
                                // reconnect burst (`wake_pending =
                                // !had_connection`) and sleep 60 s later.
                                had_connection = false;
                            }
                            embassy_futures::select::Either3::Second(()) => {
                                log!("BLE: Sync mode requested");
                                set_connection_state(ConnectionState::SyncMode);
                                break None;
                            }
                            embassy_futures::select::Either3::Third(()) => {
                                // Budget spent with no gesture — round the loop
                                // so the timeout check above fires.
                            }
                        }
                    }
                } else {
                    // No bonded device - go straight to sync mode
                    log!("BLE: No bond, auto-entering sync mode");
                    set_connection_state(ConnectionState::SyncMode);
                    None
                };

                if let Some(conn) = conn {
                    let outcome =
                        handle_connection(sd, server, bonder, &mut flash, conn, remap, None).await;
                    // Every exit from a connection drops the wire-level dedup cache.
                    // The next connection is a different peer, or the same one
                    // renegotiating, so the previous session's last report must never
                    // suppress the new session's first. Hoisted out of the match on
                    // purpose: an arm added later cannot forget it.
                    crate::ble::hid::reset_report_cache();
                    match outcome {
                        // No window is open here, so there is nothing to refuse;
                        // treat it as an ordinary lost session if it ever happens.
                        SessionOutcome::Refused
                        | SessionOutcome::Ended(DisconnectOutcome::Lost) => {
                            // Accidental — try to reconnect once. Leave
                            // had_connection unchanged so the next iteration
                            // runs Phase A.
                            log!("BLE: Connection lost, attempting reconnect");
                            transition_after_disconnect(bonder);
                        }
                        SessionOutcome::Ended(DisconnectOutcome::SyncRequested) => {
                            set_connection_state(ConnectionState::SyncMode);
                        }
                        SessionOutcome::Ended(DisconnectOutcome::HostIntentional) => {
                            // User wants the disconnect to stick (clicked
                            // Disconnect, Deck went to sleep, etc.). Skip the
                            // auto-reconnect burst — wait for an explicit
                            // wake gesture instead.
                            log!("BLE: Host-intentional disconnect, going silent");
                            had_connection = true;
                            transition_after_disconnect(bonder);
                        }
                    }
                }
            }

            ConnectionState::SyncMode => {
                // Drain any stale sync signal so it doesn't fire after disconnect
                if SYNC_MODE.signaled() {
                    SYNC_MODE.wait().await;
                }

                // ONE absolute deadline for the whole pairing session —
                // advertising, provisional stages and candidate attempts
                // alike. The 60 s used to cover only the advertising loop, so
                // any connection left it and a host that connected without
                // bonding held the window open for as long as it liked.
                let deadline = Instant::now() + Duration::from_millis(SYNC_TIMEOUT_MS);

                // The stored bond stays in flash for the whole window. Only a
                // peer that actually bonds replaces it — `on_bonded` is what
                // closes the window, so `in_window()` is also how we learn the
                // replacement landed.
                bonder.begin_window();

                let mut replaced = false;
                while Instant::now() < deadline {
                    // Saturating: `Instant - Instant` panics on a negative span,
                    // and the check above can go stale between two instructions.
                    let remaining = deadline.saturating_duration_since(Instant::now());

                    // Arm the observation *before* advertising. The pinned
                    // event loop drains every queued BLE event in one poll
                    // without yielding, and installs the security handler from
                    // the advertising callback partway through that drain — so
                    // a peer's pairing request can already have been dispatched
                    // by the time `advertise` returns. Arming after that would
                    // erase it.
                    bonder.arm_provisional();

                    // Advertise in slices so the deadline is honoured even
                    // while nothing is connecting.
                    let adv = advertise(sd, server, bonder, AdvertiseMode::SyncMode);
                    let slice = remaining.min(Duration::from_secs(5));
                    let conn = match embassy_time::with_timeout(slice, adv).await {
                        Ok(Ok(conn)) => conn,
                        Ok(Err(_)) => {
                            // An advertising error — notably the connection-count
                            // error while a refused peer's link is still tearing
                            // down — would otherwise spin this loop against the
                            // executor. Back off before re-arming.
                            Timer::after(Duration::from_millis(100)).await;
                            continue;
                        }
                        // The slice simply elapsed; re-arm immediately.
                        Err(embassy_time::TimeoutError) => continue,
                    };

                    let outcome = handle_connection(
                        sd,
                        server,
                        bonder,
                        &mut flash,
                        conn,
                        remap,
                        Some(deadline),
                    )
                    .await;
                    // Same reason as the reconnect arm above: every exit from a
                    // connection drops the wire-level dedup cache.
                    crate::ble::hid::reset_report_cache();

                    if bonder.in_window() {
                        // Refused at the provisional stage, or admitted and
                        // never bonded. Either way the stored bond is untouched
                        // and the window keeps the time it has left.
                        set_connection_state(ConnectionState::SyncMode);
                        continue;
                    }

                    // A replacement bonded. From here this is an ordinary
                    // session ending.
                    replaced = true;
                    // The bond is someone else's now, so the previous host's
                    // intent must not govern the new one. `had_connection`
                    // suppresses the reconnect advertising burst
                    // (`wake_pending = !had_connection`), and a host-intentional
                    // disconnect before this window set it. Left standing, a new
                    // host whose first session ends `Lost` — the
                    // bond-then-immediate-drop case included — would log
                    // "attempting reconnect" and then never advertise. The
                    // `HostIntentional` arm below still sets it for the *new*
                    // host.
                    had_connection = false;
                    match outcome {
                        SessionOutcome::Ended(DisconnectOutcome::SyncRequested) => {
                            set_connection_state(ConnectionState::SyncMode);
                        }
                        SessionOutcome::Ended(DisconnectOutcome::HostIntentional) => {
                            log!("BLE: Host-intentional disconnect, going silent");
                            had_connection = true;
                            transition_after_disconnect(bonder);
                        }
                        // `Refused` reaching here means the bond landed but the
                        // link ended before a session ran on it — a replacement
                        // we never got to use. Same handling as a lost link, so
                        // the new host can come back.
                        SessionOutcome::Refused
                        | SessionOutcome::Ended(DisconnectOutcome::Lost) => {
                            log!("BLE: Connection lost, attempting reconnect");
                            transition_after_disconnect(bonder);
                        }
                    }
                    break;
                }

                bonder.end_window();

                if !replaced {
                    log!("BLE: Sync mode timeout");
                    if bonder.has_bond() {
                        // The window was aborted, so the host we were already
                        // paired with is still ours. Give it one automatic
                        // retry burst — clearing `had_connection` is what
                        // forces Phase A — and then the existing silent-wait
                        // ladder takes over.
                        set_connection_state(ConnectionState::Reconnecting);
                        had_connection = false;
                    } else if crate::board::SUPPORTS_SLEEP {
                        // Never bonded at all: sleep to save power. Wake via
                        // sync button -> full reset -> auto sync mode. Routed
                        // through main so the 5 V boost goes down with us
                        // (see `request_sleep`). Never returns.
                        log!("BLE: No bond after sync timeout, requesting System Off");
                        crate::request_sleep().await;
                    } else {
                        set_connection_state(ConnectionState::Idle);
                    }
                }
            }

            ConnectionState::Connected => {
                // Shouldn't get here, but handle it
                Timer::after(Duration::from_millis(100)).await;
            }
        }
    }
}

/// How long a peer may sit connected inside a replacement window without
/// starting security before it is dropped. Long enough for a host that runs
/// service discovery first, short enough that a silent peer costs the window
/// little — and it is clamped to the window's own remaining time either way.
const PROVISIONAL_TIMEOUT_MS: u64 = 10_000;

/// How long to wait for `BLE_GAP_EVT_DISCONNECTED` after asking for a
/// disconnect. The pinned server future completes on that event, so awaiting it
/// is the actual completion signal — the bound only stops a peer that never
/// acknowledges from holding the window.
const DISCONNECT_WAIT_MS: u64 = 2_000;

/// How the provisional stage ended.
enum StageEnd {
    /// The peer started a fresh pairing.
    Admitted,
    /// The peer reused the stored key, or never started security at all.
    Refused,
    /// The link ended during the stage, so `gatt_future` has completed.
    LinkGone,
}

/// What `handle_connection` did with the peer.
enum SessionOutcome {
    /// A replacement window refused this peer — it reused the stored key, it
    /// never started security, or the window's deadline passed while it was
    /// still unbonded. The stored bond is untouched and the window continues.
    Refused,
    /// A real session ran, and ended for the given reason.
    Ended(DisconnectOutcome),
}

/// Why a session ended. Drives the "auto-reconnect or stay silent" decision
/// in the BLE task loop.
enum DisconnectOutcome {
    /// User invoked sync mode while connected — advertise discoverable. The
    /// stored bond is retained; only a new peer bonding replaces it.
    SyncRequested,
    /// Host explicitly terminated (HCI 0x13 / 0x14 / 0x15) — user wants the
    /// disconnect to stick. Don't auto-advertise; wait for a wake gesture.
    HostIntentional,
    /// Connection was lost (timeout, range, error) — accidental, OK to retry.
    Lost,
}

/// Commit anything the in-RAM bond record owes flash.
///
/// A no-op unless a generation is actually outstanding, so it is safe on every
/// exit path — which is the point: a bond can be accepted by `on_bonded` and the
/// link can end in the *same* event drain, so the session that would have
/// written it may never resume. Without a teardown that runs regardless of which
/// stage or select arm ended the connection, that replacement is lost at the
/// next reboot.
async fn flush_bond(flash: &mut nrf_softdevice::Flash, bonder: &Bonder) {
    if !bonder.save_owed() {
        return;
    }
    let generation = bonder.generation();
    if persist_bond(flash, bonder).await {
        bonder.mark_persisted(generation);
    }
}

/// Write the current bond to flash and confirm what actually landed there.
///
/// Returns `true` only when the committed record reads back as the one just
/// written. `save_bond` returning `Ok` means the driver accepted the erase and
/// the two writes, not that the page holds a valid record — the readback is
/// what makes "saved" mean durable.
///
/// Must be awaited directly, never as an arm of a `select`: the flash driver
/// panics if an in-flight operation is dropped.
async fn persist_bond(flash: &mut nrf_softdevice::Flash, bonder: &Bonder) -> bool {
    let Some((master_id, enc_info, peer_id)) = bonder.get_bond_data() else {
        return false;
    };
    let sys_attrs = bonder.get_sys_attrs();

    if crate::ble::flash_bond::save_bond(flash, &master_id, &enc_info, &peer_id, &sys_attrs)
        .await
        .is_err()
    {
        log!("BLE: Bond save FAILED (flash)");
        return false;
    }

    match crate::ble::flash_bond::load_bond() {
        Some((stored_id, stored_key, _, _)) if stored_id == master_id && stored_key == enc_info => {
            log!("BLE: Bond saved");
            true
        }
        _ => {
            log!("BLE: Bond save did not verify");
            false
        }
    }
}

/// Update connection state after a disconnection.
fn transition_after_disconnect(bonder: &Bonder) {
    if bonder.has_bond() {
        set_connection_state(ConnectionState::Reconnecting);
    } else {
        set_connection_state(ConnectionState::Idle);
    }
}

/// May this link use `HostService`? Encrypted, and not a stranger's pairing
/// refused outside a window (`Bonder::refused`). The value attributes demand
/// encryption already; the CCCD and the notify path do not, and a Just Works
/// pairing that is being refused is encrypted too.
fn host_admitted(bonder: &Bonder, conn: &nrf_softdevice::ble::Connection) -> bool {
    !bonder.refused() && GamepadServer::link_encrypted(conn)
}

/// Handle an active BLE connection.
/// Returns the reason the session ended.
async fn handle_connection(
    sd: &'static Softdevice,
    server: &'static GamepadServer,
    bonder: &'static Bonder,
    flash: &mut nrf_softdevice::Flash,
    conn: nrf_softdevice::ble::Connection,
    remap: RemapTable,
    window_deadline: Option<Instant>,
) -> SessionOutcome {
    let outcome = run_connection(sd, server, bonder, flash, conn, remap, window_deadline).await;

    // One teardown for every exit. `run_connection` has several returns and its
    // session can end on any arm of a select, so this is the only place that
    // sees them all — a bond accepted in RAM must not leave with the session.
    flush_bond(flash, bonder).await;
    bonder.release_attrs();
    // The link is gone, and nothing advertises until this returns, so no
    // event of the next connection can be dispatched before this clears.
    bonder.clear_refusal();

    outcome
}

/// The session itself. Every exit goes through `handle_connection`'s teardown.
#[expect(
    clippy::too_many_lines,
    reason = "a flat hardware bring-up / event-dispatch sequence; splitting it would scatter an order that must be read top to bottom"
)]
async fn run_connection(
    _sd: &'static Softdevice,
    server: &'static GamepadServer,
    bonder: &'static Bonder,
    flash: &mut nrf_softdevice::Flash,
    conn: nrf_softdevice::ble::Connection,
    remap: RemapTable,
    window_deadline: Option<Instant>,
) -> SessionOutcome {
    log!("BLE: Connected!");

    // If sync was requested before we got here, honor it immediately
    if SYNC_MODE.signaled() {
        SYNC_MODE.wait().await;
        log!("BLE: Sync requested during connection setup");
        return SessionOutcome::Ended(DisconnectOutcome::SyncRequested);
    }

    // Likewise a profile switch, and for the same reason it must come before
    // `host_vmu::reset` below: the triple-press paused write admission when
    // it passed its drain check, and `reset` lifts that pause. A switch taken
    // while disconnected reaches here unhandled (the reconnect loop does not
    // look), and a WRITE the new session acked before the reset would be lost.
    if PROFILE_CHANGE.signaled() {
        let next = PROFILE_CHANGE.wait().await;
        log!(
            "PROFILE: Switching to {} (at connection setup)",
            core::str::from_utf8(next.profile().vmu_label).unwrap_or("?")
        );
        let _ = crate::ble::prefs::save_profile(flash, next).await;
        cortex_m::peripheral::SCB::sys_reset();
    }

    // clear the VMU storage service, here rather than where
    // `host_lcd::reset` is called.
    //
    // The LCD ingress only has to be clear before the *poll loop* draws, so
    // main's Phase 2 entry is early enough for it. This one can be *notified*
    // — by `vmu_future` below, which lives and dies with this connection — so
    // it has to be clear before the first drain, which is well before main
    // reaches Phase 2. A reply left half-sent by the previous session would
    // otherwise reach the new host as a DATA for a block it never asked for.
    //
    // Nothing can have arrived on the service yet: `gatt_server::run` is
    // built below, so no write has been dispatched to `accept_write`.
    //
    // A later change moved that server ahead of the provisional stage, so this now
    // runs at the top of every connection, refused ones included. That is the
    // direction the invariant wanted: a peer that writes here while it is
    // still being classified never reaches the poll loop — `Connected` is set
    // only on admission — and whatever it left is cleared by the next
    // connection rather than served to it.
    //
    // "Cleared" is the read side only. The generation (`epoch`, `card`), the
    // staged write queue and every WRITTEN owed survive a session start on
    // purpose: a BLE disconnect is not a card change, and the dongle's
    // reconnect rule reads an unchanged `epoch` as "nothing was lost". Whether
    // the port stayed watched through the gap is the poll loop's call
    // (`host_vmu::unwatched`), not this one's.
    crate::ble::host_vmu::reset();

    // Initialise this connection's attribute table **before** the server that
    // can modify it exists, and never again for this link.
    //
    // `set_sys_attrs(conn, None)` does not mean "leave them alone" — it clears
    // them. Doing this at admission instead destroyed real work: `on_bonded`
    // zeroes `sys_attrs_len`, so a peer that bonded *and* subscribed during the
    // provisional stage — both reachable in one event drain — had its CCCD
    // erased the moment it was admitted, and every HID notify then failed until
    // the session hit `MAX_NOTIFY_FAILURES` and dropped.
    //
    // Here it is correct for both peers: `load_sys_attrs` resolves the address
    // itself, restoring the retained host's snapshot and giving anyone else a
    // clean slate.
    bonder.load_sys_attrs(&conn);

    // Run GATT server while connected. Two writes are acted on; everything
    // else the host writes is accepted and ignored.
    //
    //  - The HID rumble Output report (Report ID 0x03): forward the commanded
    //    intensity to the board's motor.
    //  - The vendor LCD frame : hand the bytes to `host_lcd`, which
    //    copies them into a static and returns. No bus transaction, no await —
    //    this callback runs inside the GATT event dispatch and every Maple
    //    transaction belongs to the main loop's quiet-window pacer.
    let gatt_future = gatt_server::run(&conn, server, |event| match event {
        crate::ble::hid::GamepadServerEvent::Hid(
            crate::ble::hid::HidServiceEvent::RumbleWrite(data),
        ) => {
            // Xbox rumble report: byte 0 = enable mask, bytes 1..5 = motor
            // magnitudes. Use the strongest commanded magnitude. ⚠ VERIFY layout.
            let intensity = if data[0] != 0 {
                data[1..5].iter().copied().max().unwrap_or(0)
            } else {
                0
            };
            crate::RUMBLE_LEVEL.signal(intensity);
        }
        crate::ble::hid::GamepadServerEvent::Host(
            crate::ble::hid::HostServiceEvent::LcdFrameWrite(data),
        ) if host_admitted(bonder, &conn) => crate::ble::host_lcd::accept_write(&data),
        // a host block-read request. Same contract as the LCD
        // frame — copy into a static and return. The reply is produced by
        // the poll loop, which owns the bus, and notified by `vmu_future`
        // below, which owns the connection.
        crate::ble::hid::GamepadServerEvent::Host(
            crate::ble::hid::HostServiceEvent::VmuDownWrite(data),
        ) if host_admitted(bonder, &conn) => crate::ble::host_vmu::accept_write(&data),
        // Protocol v1: STATUS on subscribe. The dongle waits for it before it
        // asks for anything, so it must not wait for a change that may not
        // come. Gated on the refusal only, not on encryption: some stacks write
        // the CCCD before the link is encrypted, and that host is still owed its
        // STATUS. This only marks it due; `vmu_future` sends nothing until the
        // link is admitted, so a stranger still receives nothing.
        crate::ble::hid::GamepadServerEvent::Host(
            crate::ble::hid::HostServiceEvent::VmuUpCccdWrite { notifications },
        ) if notifications && !bonder.refused() => {
            crate::ble::host_vmu::note_subscribed();
        }
        _ => {}
    });
    let mut gatt_future = core::pin::pin!(gatt_future);

    // --- Provisional stage (replacement window only) ----------------------
    //
    // A peer that connects during a pairing window is not a session yet. The
    // link is up and the GATT server answers — some hosts start security only
    // after touching a protected characteristic, and one built *after* this
    // point would drop their in-flight ATT requests — but nothing downstream
    // runs: no `Connected` state, no HID setup, no notify loop, no conn-param
    // update, so a rejected peer never spins up the controller path or the
    // power/UI behaviour that follows from it.
    //
    // Classification cannot be a precondition of `request_security`, because a
    // host may simply wait for the peripheral to ask. So the stage asks, then
    // waits — bounded, so a peer that connects and stays silent cannot hold the
    // window open.
    let provisional = window_deadline.is_some() && bonder.in_window();
    if let Some(deadline) = window_deadline.filter(|_| provisional) {
        let watch = async {
            Timer::after(Duration::from_millis(100)).await;
            if bonder.observed() == Observed::Nothing {
                // Only ask if the peer has not already started something.
                let _ = conn.request_security();
            }
            loop {
                match bonder.observed() {
                    // The peer wants a fresh pairing. Whether it is a new host
                    // or the old one repairing, the policy is the same: admit
                    // it. The callbacks name a procedure, not an intent.
                    Observed::FreshPairing => break true,
                    // The peer wants to reuse the stored key. Refuse — this
                    // window exists to offer the slot to someone else.
                    Observed::OldKeyReuse => break false,
                    Observed::Nothing => Timer::after(Duration::from_millis(20)).await,
                }
            }
        };

        let stage = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(PROVISIONAL_TIMEOUT_MS));
        #[expect(
            clippy::match_same_arms,
            reason = "distinct stage outcomes that happen to refuse alike; merging them \
                      would hide which cases the stage actually covers"
        )]
        let ended = match embassy_futures::select::select3(
            gatt_future.as_mut(),
            watch,
            Timer::after(stage),
        )
        .await
        {
            // The link ended inside the stage. `gatt_future` has completed, so
            // there is no session left to run on it and it must not be polled
            // again.
            embassy_futures::select::Either3::First(_) => StageEnd::LinkGone,
            embassy_futures::select::Either3::Second(true) => StageEnd::Admitted,
            embassy_futures::select::Either3::Second(false) => StageEnd::Refused,
            // Connected but silent. Dropping it costs the window nothing but
            // the stage timeout; its remaining time is not reset.
            embassy_futures::select::Either3::Third(()) => StageEnd::Refused,
        };

        // A bond can be accepted in the very same event drain that carried the
        // peer's last message, so a stage that ended without an admission is
        // not automatically a refusal: `on_bonded` closes the window, and a
        // closed window means the replacement is already ours.
        let replacement_landed = !bonder.in_window();

        match ended {
            // Bonded or not, the link is gone. The teardown in
            // `handle_connection` commits anything it left owing, and the outer
            // loop reads `in_window()` to tell the two apart.
            StageEnd::LinkGone => {
                log!("BLE: Sync window: link ended during classification");
                return SessionOutcome::Refused;
            }
            StageEnd::Refused if !replacement_landed => {
                log!("BLE: Sync window: peer refused, window continues");
                let _ = conn.disconnect();
                // `disconnect()` only *starts* termination. The server future
                // completes on BLE_GAP_EVT_DISCONNECTED, so awaiting it is the
                // completion signal — advertising again before the slot frees
                // just fails with a connection-count error.
                let _ = embassy_time::with_timeout(
                    Duration::from_millis(DISCONNECT_WAIT_MS),
                    gatt_future.as_mut(),
                )
                .await;
                return SessionOutcome::Refused;
            }
            // Admitted, or a bond that beat the watcher to the select. Either
            // way the link is up and the session runs.
            //
            // The attribute hold stays on until `on_bonded` releases it: a peer
            // that was admitted and then failed to pair is still a refused
            // session and must not rewrite the retained host's CCCDs.
            StageEnd::Refused | StageEnd::Admitted => {
                log!("BLE: Sync window: candidate admitted");

                // Admission means pairing *started*. `FreshPairing` is raised by
                // a SEC_PARAMS_REQUEST, which proves intent and nothing more, so
                // publishing `Connected` here would activate the controller and
                // the display for a candidate that may still fail — and on
                // failure hand them straight back to `SyncMode`, rewriting the
                // VMU splash on every attempt. A host retrying in a loop turned
                // that into a visible flicker at the bench.
                //
                // So wait for the bond itself, with GATT and security still
                // running underneath — some hosts subscribe before they bond,
                // and a server torn down here would drop their in-flight ATT
                // requests. `on_bonded` closes the window, which is the signal.
                // `StageEnd::Refused` reaching this arm already bonded, so its
                // wait returns at once.
                if bonder.in_window() {
                    let settle = deadline.saturating_duration_since(Instant::now());
                    let wait_for_bond = async {
                        while bonder.in_window() {
                            Timer::after(Duration::from_millis(20)).await;
                        }
                    };
                    match embassy_futures::select::select3(
                        gatt_future.as_mut(),
                        wait_for_bond,
                        Timer::after(settle),
                    )
                    .await
                    {
                        // Link gone before the bond. The teardown commits
                        // anything owed; the outer loop reads `in_window()`.
                        embassy_futures::select::Either3::First(_) => {
                            log!("BLE: Sync window: candidate left before bonding");
                            return SessionOutcome::Refused;
                        }
                        embassy_futures::select::Either3::Second(()) => {}
                        // Deadline. Not the same thing as "never bonded": the
                        // watcher polls every 20 ms, so a bond can land while it
                        // sleeps and the timer still win the select. Re-read the
                        // window before concluding anything, exactly as the
                        // classification stage does above — dropping a peer that
                        // had in fact bonded would throw away the replacement.
                        embassy_futures::select::Either3::Third(()) => {
                            if bonder.in_window() {
                                log!("BLE: Sync window: candidate admitted but never bonded");
                                let _ = conn.disconnect();
                                let _ = embassy_time::with_timeout(
                                    Duration::from_millis(DISCONNECT_WAIT_MS),
                                    gatt_future.as_mut(),
                                )
                                .await;
                                return SessionOutcome::Refused;
                            }
                            log!("BLE: Sync window: bond landed as the deadline expired");
                        }
                    }
                }
            }
        }
    }

    set_connection_state(ConnectionState::Connected);

    // This session is real now, so it owns the attribute record. Claiming it
    // here rather than on connect is what keeps a refused peer's late
    // `DISCONNECTED` from saving over the retained host's snapshot.
    // Attributes were initialised before the server was built; loading them a
    // second time would clear whatever the peer has subscribed to since.
    bonder.own_attrs(&conn);
    Timer::after(Duration::from_millis(100)).await;
    if !provisional {
        // The provisional stage already asked.
        let _ = conn.request_security();
    }

    // Request the fastest connection interval Apple will consider for a BLE HID
    // accessory. Units are 1.25 ms.
    //
    // The previous request (min 7 = 8.75 ms, max 9 = 11.25 ms) was **rejected on
    // every macOS connection** — it broke two of Apple's rules (QA1931):
    //
    //   - Interval Min >= 15 ms (multiples of 15 ms)
    //   - Interval Min + 15 ms <= Interval Max  (Interval Max == 15 ms is allowed)
    //   - Interval Max * (Slave Latency + 1) <= 2 s
    //   - Interval Max * (Slave Latency + 1) * 3 < connSupervisionTimeout
    //   - Slave Latency <= 30
    //   - 2 s <= connSupervisionTimeout <= 6 s
    //
    // with the exception that matters here: "If Bluetooth Low Energy HID is one
    // of the connected services of an accessory, connection interval down to
    // 11.25 ms may be accepted by the Apple product."
    //
    // 8.75 ms is below even that HID floor, and 8.75 + 15 > 11.25 broke the span
    // rule. Apple: non-compliant requests "may be rejected, or the stability and
    // the performance of the connection may be compromised". So the host ignored
    // us and imposed its own 15 ms — which is exactly the 15.0 ms median every
    // `hid_capture.py` run has ever reported. The old `~100Hz` comment was
    // aspiration; 66.6 Hz (1000/15) was always the host's cap, not ours.
    //
    // **Both Apple-oriented alternatives were measured and neither moved macOS**
    // (2026-07-27, `hid_capture.py --history`):
    //
    //   `826aef9`  min 11.25 / max 15 ms     -> median 15.0 ms  (x3)
    //   `fe99c1a`  min 11.25 / max 26.25 ms  -> median 15.0 ms  (x2)
    //              (fully rule-compliant: 11.25 + 15 = 26.25 exactly)
    //
    // So compliance was never the blocker — macOS wants 15 ms for this device
    // and takes it whether or not the request is legal. Note 15 ms sits *inside*
    // the `fe99c1a` range, so that one may even have been accepted-and-chosen
    // rather than refused; the two are indistinguishable from outside, and no
    // further parameter tuning can separate them (see the BUSY note below).
    //
    // Reverted to the original range because it has the **tightest ceiling of
    // the three** — a host that honours it cannot grant worse than 11.25 ms,
    // where the "compliant" range legally permits 26.25 ms (38 Hz):
    //
    //   this      8.75 - 11.25 ms  ->   89 - 114 Hz
    //   826aef9  11.25 - 15    ms  -> 66.6 -  89 Hz
    //   fe99c1a  11.25 - 26.25 ms  ->   38 -  89 Hz
    //
    // All three are identical on macOS, so the difference only shows on hosts
    // that actually honour peripheral requests — BlueZ and most non-Apple
    // centrals do. This adapter is not macOS-only, and optimising for the one
    // host that ignores us would cost real rate everywhere else.
    //
    // Known cost: this violates two of Apple's rules (Min >= 11.25 ms for HID,
    // and Min + 15 ms <= Max), and Apple warns non-compliant requests "may be
    // rejected, or the stability and the performance of the connection may be
    // compromised". Accepted deliberately — the whole project has run on these
    // values with stable connections, IQR 0.7-1.1 ms and zero reversals across
    // every capture. Rejection costs nothing: the host default is the same
    // 15 ms the compliant requests obtained.
    //
    // The poll pacer locks one Maple poll to the head of each inter-event
    // quiet window (radio-notification gate), so every connection event
    // finds fresh controller state at any interval the host grants — see
    // the POLL_PERIOD_MS docs in main.rs.
    Timer::after(Duration::from_millis(500)).await;
    if let Some(handle) = conn.handle() {
        let conn_params = nrf_softdevice::raw::ble_gap_conn_params_t {
            min_conn_interval: 7, // 8.75ms
            max_conn_interval: 9, // 11.25ms — tightest ceiling of the options tried
            slave_latency: 0,
            conn_sup_timeout: 400, // 4000ms (within Apple's 2-6s window)
        };
        // SAFETY: Connection handle is valid (checked above). conn_params is
        // a well-formed struct on the stack, passed as a const pointer.
        let rc = unsafe {
            nrf_softdevice::raw::sd_ble_gap_conn_param_update(
                handle,
                (&raw const conn_params).cast_mut(),
            )
        };
        // `rc == 0` means the request was *queued*, not accepted — the outcome
        // arrives later as BLE_GAP_EVT_CONN_PARAM_UPDATE. Publishing the raw
        // code is the point: NRF_ERROR_BUSY (17) here would mean the request
        // never went out at all, which no host-side measurement can reveal.
        #[cfg(feature = "connparam-debug")]
        crate::publish_connparam_rc(rc);
        if rc != 0 {
            log!("BLE: Conn param update not queued, rc={}", rc);
        }
    }

    // Notification sender - sends HID reports at fixed 125Hz interval.
    // Reads state changes promptly (so the Signal is cleared before the next
    // poll overwrites it), but always waits for the timer before sending to
    // maintain a steady cadence that matches the BLE connection interval.
    let notify_future = async {
        // Wait for client to discover services and subscribe
        Timer::after(Duration::from_millis(crate::SERVICE_DISCOVERY_DELAY_MS)).await;

        let mut current_state = ControllerState::default();
        let mut budget = NotifyBudget::new(crate::MAX_NOTIFY_FAILURES);
        let mut guide_chord = GuideChord::default();

        loop {
            // Read any pending state change promptly, then wait for send timer.
            if let Some(state) = RAW_CONTROLLER_STATE.try_take() {
                current_state = state;
                #[cfg(feature = "poll-period-debug")]
                crate::poll_period::note_fresh_sample();
            }

            // Fixed-rate send at ~125Hz — matches Xbox cadence and BLE conn interval
            Timer::after(Duration::from_millis(crate::NOTIFY_INTERVAL_MS)).await;

            // Grab any state that arrived during the wait
            if let Some(state) = RAW_CONTROLLER_STATE.try_take() {
                current_state = state;
                #[cfg(feature = "poll-period-debug")]
                crate::poll_period::note_fresh_sample();
            }

            // One conversion for the map and the Guide chord (remap design
            // v2 §2.2): the source-keyed map is applied with typed reducers
            // and the L+R+Start chord's constituents are excluded at source
            // level before fan-in — the same function the config
            // personality previews as LiveOutput. The map was loaded once
            // before this task spawned and cannot change until a reset
            // (§2.3): the only writer runs in the config boot.
            let (report, chord) = current_state.to_gamepad_report_with(
                &remap,
                &mut guide_chord,
                Instant::now().as_millis(),
            );
            if chord.rising_edge {
                // Best-effort, fire-and-forget: ask the main loop to flash the
                // VMU home glyph. Single non-blocking atomic store; the main
                // loop may drop it. Never touches the controller path.
                crate::GUIDE_GLYPH_PENDING.store(true, core::sync::atomic::Ordering::Relaxed);
            }
            let report_bytes = GamepadServer::serialize_report_for_active_profile(&report);
            let _ = server.hid.report_set(&report_bytes);
            // Which failures count toward hanging up is `NotifyBudget`'s rule:
            // on an encrypted link only those retrying cannot fix;
            // on an unencrypted one nothing is sent and every due report
            // counts, because the CCCDs are open and the peer is not a host we
            // serve. Read per tick — encryption can land after the loop starts.
            let encrypted = GamepadServer::link_encrypted(&conn);
            let outcome = if NotifyBudget::may_send(encrypted) {
                GamepadServer::notify_outcome(server.send_report(&conn, &report))
            } else {
                Outcome::Withheld
            };
            if budget.record(encrypted, outcome) == Verdict::HangUp {
                log!("BLE: Too many notify failures, disconnecting");
                break;
            }
        }
    };

    // Update battery level in the BLE service when signaled. Boards without a
    // gauge never signal BATTERY_LEVEL, so this future just stays pending (inert)
    // on those builds. 0xFF = charging (don't update percentage), else 0-100%.
    let battery_future = async {
        loop {
            let level = BATTERY_LEVEL.wait().await;
            if level != 0xFF {
                let _ = server.battery.battery_level_set(&level);
                let _ = server.battery.battery_level_notify(&conn, &level);
            }
        }
    };

    // notify whatever the VMU storage service has waiting — block
    // phases from the poll loop, and STATUS.
    //
    // It lives here because the connection does: the poll loop owns the Maple
    // bus and must never block on the radio, and this task owns `conn` and
    // must never touch the bus. `with_next` advances only on a notify that was
    // accepted, so a full queue costs a tick rather than a lost phase — a
    // dropped phase would strand the dongle waiting for a fourth DATA forever.
    let vmu_future = async {
        loop {
            Timer::after(Duration::from_millis(crate::VMU_DRAIN_INTERVAL_MS)).await;
            // Nothing goes out to a link that is not the bonded host's. WRITTEN
            // is delivered once and then its slot is freed, so a notify to a
            // stranger (or to a peer that subscribed before encrypting — the
            // CCCD itself is open) would lose it for the dongle for good.
            if !host_admitted(bonder, &conn) {
                continue;
            }
            // One block's four phases at most per tick, stopping at the first
            // notification the SoftDevice will not take.
            for _ in 0..4 {
                let sent = crate::ble::host_vmu::with_next(|bytes| {
                    let mut msg: heapless::Vec<u8, { maple_protocol::host_vmu_io::MSG_MAX }> =
                        heapless::Vec::new();
                    if msg.extend_from_slice(bytes).is_err() {
                        return false;
                    }
                    server.host.vmu_up_notify(&conn, &msg).is_ok()
                });
                if !sent {
                    break;
                }
            }
        }
    };
    // Wait for sync mode request — disconnects active connection
    let sync_future = SYNC_MODE.wait();

    // Run all until one completes (connection drops or sync requested). The
    // battery future is inert on boards without a gauge (never signaled).
    let main_futures = embassy_futures::select::select4(
        gatt_future.as_mut(),
        notify_future,
        battery_future,
        vmu_future,
    );
    let mut session_end =
        core::pin::pin!(embassy_futures::select::select(main_futures, sync_future));

    // Flash work — bond persistence and the profile switch — is driven from
    // this future's own body and is never an arm of the session select. The
    // pinned flash driver arms a `DropBomb`: dropping an in-flight erase or
    // write *panics*, and a disconnect or a sync press could previously cancel
    // the save future in the middle of one. Everything below the select is
    // awaited directly here, so nothing can drop it — `handle_connection` is
    // awaited by `ble_task`, never selected against.
    //
    // The bond is still written early rather than only at disconnect, so it
    // survives an unexpected sleep or reset. What triggers the write is the
    // bond *generation*, not a poll for "is there bond data yet": during a
    // replacement window there always is — the retained one — and a poll would
    // write that straight back, stop watching, and miss the replacement until
    // disconnect.
    let mut tick: u32 = 0;
    let sync_requested = loop {
        let tick_timer = Timer::after(Duration::from_millis(100));

        match embassy_futures::select::select(tick_timer, session_end.as_mut()).await {
            embassy_futures::select::Either::First(()) => {}
            embassy_futures::select::Either::Second(ended) => {
                break match ended {
                    embassy_futures::select::Either::First(inner) => {
                        match inner {
                            embassy_futures::select::Either4::First(_gatt_result) => {
                                log!("BLE: Disconnected (GATT: {:?})", _gatt_result);
                            }
                            embassy_futures::select::Either4::Second(()) => {
                                log!("BLE: Disconnected (notify failure)");
                            }
                            embassy_futures::select::Either4::Third(()) => {
                                log!("BLE: Disconnected (battery task ended)");
                            }
                            embassy_futures::select::Either4::Fourth(()) => {
                                // Both arms of `vmu_future` loop forever, so this is
                                // unreachable rather than merely unexpected.
                                log!("BLE: Disconnected (VMU egress task ended)");
                            }
                        }
                        false
                    }
                    embassy_futures::select::Either::Second(()) => {
                        log!("BLE: Sync mode requested, disconnecting");
                        true
                    }
                };
            }
        }

        // The window's deadline has to reach in here too. A candidate that was
        // admitted and then never bonded would otherwise hold the session — and
        // so the window — open indefinitely. Checked on the tick rather than as
        // a select arm, because this is the one place where nothing is in
        // flight and the session can be abandoned safely.
        if bonder.in_window() && window_deadline.is_some_and(|d| Instant::now() >= d) {
            log!("BLE: Sync window expired with the candidate unbonded");
            let _ = conn.disconnect();
            Timer::after(Duration::from_millis(500)).await;
            return SessionOutcome::Refused;
        }

        // Nothing below this point can be cancelled.
        if PROFILE_CHANGE.signaled() {
            let next = PROFILE_CHANGE.wait().await;
            log!(
                "PROFILE: Switching to {}",
                core::str::from_utf8(next.profile().vmu_label).unwrap_or("?")
            );
            let _ = crate::ble::prefs::save_profile(flash, next).await;
            cortex_m::peripheral::SCB::sys_reset();
        }

        // Once a second, refresh the attribute snapshot. It bumps the
        // generation only when the bytes actually changed, so an idle session
        // never reaches flash — but a session that is reset unexpectedly has
        // its CCCDs already saved rather than only at the disconnect that never
        // comes.
        tick = tick.wrapping_add(1);
        if tick.is_multiple_of(10) {
            bonder.save_sys_attrs(&conn);
        }

        if bonder.save_owed() {
            let generation = bonder.generation();
            if persist_bond(flash, bonder).await {
                // A save whose generation has since been superseded must not be
                // recorded as current, or the newer bond would never be written.
                bonder.mark_persisted(generation);
            }
        }
    };

    // Explicitly disconnect so the host sees a clean GAP termination
    // before we start advertising in sync mode.
    if sync_requested {
        let _ = conn.disconnect();
        // Give the host time to process the disconnect
        Timer::after(Duration::from_millis(1000)).await;
        return SessionOutcome::Ended(DisconnectOutcome::SyncRequested);
    }

    // The attribute snapshot is usually only complete by now — the host
    // subscribes after bonding — so this is where the CCCDs actually land.
    bonder.save_sys_attrs(&conn);
    if bonder.save_owed() {
        let generation = bonder.generation();
        if persist_bond(flash, bonder).await {
            bonder.mark_persisted(generation);
        }
    }
    Timer::after(Duration::from_millis(500)).await;

    // Read the HCI disconnect reason from the (now-disconnected) Connection
    // and classify. Host-initiated termination (0x13 user, 0x14 low resources,
    // 0x15 power off) means the user wants the disconnect to stick. Anything
    // else (timeout, error) is treated as accidental — eligible for auto retry.
    let reason = conn.disconnect_reason();
    log!("BLE: Disconnect reason = {:?}", reason);
    SessionOutcome::Ended(match reason {
        Some(
            HciStatus::REMOTE_USER_TERMINATED_CONNECTION
            | HciStatus::REMOTE_DEV_TERMINATION_DUE_TO_LOW_RESOURCES
            | HciStatus::REMOTE_DEV_TERMINATION_DUE_TO_POWER_OFF,
        ) => DisconnectOutcome::HostIntentional,
        _ => DisconnectOutcome::Lost,
    })
}
