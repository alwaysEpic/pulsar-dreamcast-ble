// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Isolated, unbonded BLE configuration personality (ADR-017; remap design
//! v2 §4).
//!
//! This deliberately shares no GATT service or connection handler with HID.
//! Exactly one of [`ConfigServer`] or `GamepadServer` is registered per
//! boot; services registered with the SoftDevice cannot be removed at
//! runtime. The protocol state machine lives host-tested in
//! `maple_protocol::config_protocol`; this module is only the transport:
//! GATT callbacks enqueue writes, the config task drives the machine,
//! performs the preview/flash work it requests, and resets back to the
//! unchanged HID personality on exit, disconnect, advertising lapse or
//! deadline expiry.

#![expect(
    clippy::redundant_else,
    reason = "expanded from the nrf-softdevice GATT macros"
)]
#![expect(
    clippy::unnecessary_semicolon,
    reason = "expanded from the nrf-softdevice GATT macros"
)]
#![expect(
    clippy::missing_errors_doc,
    reason = "the generated characteristic accessors and the small in-crate initializer all return the SoftDevice's SetValueError directly"
)]

use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use heapless::Vec;
use maple_protocol::config_protocol::{
    encode_live_input, encode_live_output, Action, ConfigProtocol, Expiry, Response, WorkKind,
};
use maple_protocol::guide_chord::GuideChord;
use maple_protocol::remap::RemapTable;
use nrf_softdevice::ble::advertisement_builder::{
    Flag, LegacyAdvertisementBuilder, LegacyAdvertisementPayload, ServiceList,
};
use nrf_softdevice::ble::gatt_server::{self, SetValueError};
use nrf_softdevice::ble::{peripheral, Address, AddressType, Connection};
use nrf_softdevice::Softdevice;

use crate::ble::prefs::LoadedPrefs;
use crate::ble::{set_connection_state, ConnectionState};
use crate::maple::ControllerState;
use crate::RAW_CONTROLLER_STATE;

/// Vendor service UUID advertised to Web Bluetooth.
pub const CONFIG_SERVICE_UUID: u128 = 0x7e1d_0001_4a7b_4f15_9d6c_5a0b_6c5e_8f40;

/// Advertising window; lapse resets to HID (design v2 §4.5).
const ADVERTISE_TIMEOUT_10MS: u16 = 6000; // 60 s in 10 ms units
/// Live notification cadence (~50 Hz).
const LIVE_NOTIFY_INTERVAL: Duration = Duration::from_millis(20);
/// One connection event's grace for a final notification to leave the
/// radio before the reset (§4.3, Exit).
const RESET_GRACE: Duration = Duration::from_millis(100);

/// A queued client write, GATT callback → config task. The callback only
/// enqueues (§4.3: nothing is signalled through an overwritable `Signal`);
/// the task consumes in order and does the flash work in the future that
/// owns `&mut Flash`.
enum Command {
    Control(Vec<u8, 5>),
    Map(Vec<u8, { RemapTable::LEN }>),
}

/// Capacity 4 is generous for a single-flight protocol whose every command
/// is answered before the next is usefully sent; a flood that overflows it
/// loses only its own excess writes.
static COMMANDS: Channel<ThreadModeRawMutex, Command, 4> = Channel::new();

#[nrf_softdevice::gatt_service(uuid = "7e1d0001-4a7b-4f15-9d6c-5a0b6c5e8f40")]
pub struct ConfigService {
    /// §4.2 `Info`: `[proto_major, proto_minor, flags, profile_id, map_len,
    /// att_payload, stored_schema, source, idle_s, abs_min]`.
    #[characteristic(uuid = "7e1d0002-4a7b-4f15-9d6c-5a0b6c5e8f40", read)]
    pub info: [u8; 10],

    /// §4.2 `LiveInput`: raw source sample
    /// `[buttons_le16, L, R, stick_x, stick_y, seq_le16]`.
    #[characteristic(uuid = "7e1d0003-4a7b-4f15-9d6c-5a0b6c5e8f40", notify)]
    pub live_input: [u8; 8],

    /// §4.2/§4.3 `Control`: 4-byte requests in, 5-byte responses out on the
    /// notify. `Vec`, never `[u8; N]` — the array impl zero-pads a short
    /// write, and a padded 1-byte `[0x05]` would be a valid `Ping(0)`. The
    /// value holds the 5-byte response, so the ATT maximum is 5; length
    /// validation still requires exactly 4 for a request.
    #[characteristic(uuid = "7e1d0004-4a7b-4f15-9d6c-5a0b6c5e8f40", write, notify)]
    pub control: Vec<u8, 5>,

    /// §4.2 `StoredMap`: the 20-byte **effective durable** map — what the
    /// next HID boot will load. Never a preview; read-only on purpose.
    #[characteristic(uuid = "7e1d0005-4a7b-4f15-9d6c-5a0b6c5e8f40", read)]
    pub stored_map: [u8; RemapTable::LEN],

    /// §4.2 `Map`: the candidate for the armed transaction, exactly 20
    /// bytes. Write-only: the SoftDevice accepts a write into the attribute
    /// before `on_write` runs, so a readable Map would show an invalid
    /// candidate to the next reader. Length-preserving `Vec` (see Control).
    #[characteristic(uuid = "7e1d0006-4a7b-4f15-9d6c-5a0b6c5e8f40", write)]
    pub map: Vec<u8, { RemapTable::LEN }>,

    /// §4.2 `LiveOutput`: the logical report computed on-device under the
    /// active map — candidate if previewing, else stored —
    /// `[buttons_le16, hat, lt_le16, rt_le16, lx_le16, ly_le16, seq_le16]`.
    #[characteristic(uuid = "7e1d0007-4a7b-4f15-9d6c-5a0b6c5e8f40", notify)]
    pub live_output: [u8; 13],
}

#[nrf_softdevice::gatt_server]
pub struct ConfigServer {
    pub config: ConfigService,
}

impl ConfigServer {
    /// Initialize readable state before advertising: `Info` (no transaction,
    /// nothing dirty) and the effective stored map.
    pub fn init(&self, prefs: &LoadedPrefs) -> Result<(), SetValueError> {
        let protocol = ConfigProtocol::new(prefs.remap, 0);
        self.config
            .info_set(&protocol.info(prefs.profile_id as u8, prefs.source))?;
        self.config
            .stored_map_set(&protocol.stored_map().to_bytes())?;
        Ok(())
    }
}

/// Derive the isolated, deterministic random-static configuration address.
///
/// The least-significant byte is toggled with a project constant and the two
/// high bits of the most-significant byte are forced to `11`, as required for
/// a static random address. This is reversible, deterministic, and always
/// differs from the normal address.
#[must_use]
pub fn derive_config_address(normal: Address) -> Address {
    let mut bytes = normal.bytes();
    bytes[0] ^= 0x5A;
    bytes[5] |= 0xC0;
    let random_part_all_zero = bytes[..5].iter().all(|byte| *byte == 0) && bytes[5] == 0xC0;
    let random_part_all_one = bytes[..5].iter().all(|byte| *byte == 0xFF) && bytes[5] == 0xFF;
    if random_part_all_zero {
        bytes[0] = 1;
    } else if random_part_all_one {
        bytes[0] = 0xFE;
    }
    Address::new(AddressType::RandomStatic, bytes)
}

/// Switch to the isolated address before advertising or connecting.
pub fn activate_config_address(sd: &Softdevice) {
    let normal = nrf_softdevice::ble::get_address(sd);
    let config = derive_config_address(normal);
    nrf_softdevice::ble::set_address(sd, &config);
}

static ADV_DATA: LegacyAdvertisementPayload = LegacyAdvertisementBuilder::new()
    .flags(&[Flag::GeneralDiscovery, Flag::LE_Only])
    .full_name("Pulsar Configure")
    .build();

static SCAN_DATA: LegacyAdvertisementPayload = LegacyAdvertisementBuilder::new()
    .services_128(ServiceList::Complete, &[CONFIG_SERVICE_UUID.to_le_bytes()])
    .build();

/// Enqueue a client write for the config task; drop everything else
/// (CCCD subscriptions need no action here).
fn enqueue(event: ConfigServerEvent) {
    let command = match event {
        ConfigServerEvent::Config(ConfigServiceEvent::ControlWrite(value)) => {
            Command::Control(value)
        }
        ConfigServerEvent::Config(ConfigServiceEvent::MapWrite(value)) => Command::Map(value),
        ConfigServerEvent::Config(_) => return,
    };
    let _ = COMMANDS.try_send(command);
}

/// Set and notify a 5-byte Control response.
fn notify_control(server: &ConfigServer, conn: &Connection, resp: Response) {
    let value: Vec<u8, 5> = Vec::from_slice(&resp).unwrap_or_default();
    let _ = server.config.control_set(&value);
    let _ = server.config.control_notify(conn, &value);
}

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

/// Advertise and serve the unbonded configuration personality once.
///
/// No `Bonder`, security request, system-attribute restore, HID or battery
/// notification, or bond-save path exists on this boot. Advertisement
/// lapse, disconnect, Exit, arm/idle/absolute deadlines and power loss all
/// return to the unchanged HID boot by reset.
#[embassy_executor::task]
pub async fn config_task(
    sd: &'static Softdevice,
    server: &'static ConfigServer,
    prefs: LoadedPrefs,
) {
    set_connection_state(ConnectionState::SyncMode);

    let config = peripheral::Config {
        interval: 32,
        timeout: Some(ADVERTISE_TIMEOUT_10MS),
        ..Default::default()
    };
    let adv = peripheral::ConnectableAdvertisement::ScannableUndirected {
        adv_data: &ADV_DATA,
        scan_data: &SCAN_DATA,
    };

    let Ok(conn) = peripheral::advertise_connectable(sd, adv, &config).await else {
        cortex_m::peripheral::SCB::sys_reset();
    };
    set_connection_state(ConnectionState::Connected);

    // The flash handle lives in this future for the whole session; the
    // protocol core hands it work through Action::StartWork.
    let mut flash = nrf_softdevice::Flash::take(sd);
    let mut protocol = ConfigProtocol::new(prefs.remap, now_ms());
    let profile_byte = prefs.profile_id as u8;
    let _ = server
        .config
        .info_set(&protocol.info(profile_byte, prefs.source));
    let _ = server
        .config
        .stored_map_set(&protocol.stored_map().to_bytes());

    let gatt = gatt_server::run(&conn, server, enqueue);
    let engine = engine(server, &conn, &mut flash, &mut protocol, prefs);

    match select(gatt, engine).await {
        Either::First(_) | Either::Second(()) => cortex_m::peripheral::SCB::sys_reset(),
    }
}

/// Drive the protocol: consume queued writes, execute the work they arm,
/// publish `LiveInput`/`LiveOutput`, and enforce the deadlines. Returns
/// when the session must end (the caller resets).
async fn engine(
    server: &ConfigServer,
    conn: &Connection,
    flash: &mut nrf_softdevice::Flash,
    protocol: &mut ConfigProtocol,
    prefs: LoadedPrefs,
) {
    let profile_byte = prefs.profile_id as u8;
    let mut chord = GuideChord::default();
    let mut live_seq: u16 = 0;
    let mut current = ControllerState::default();

    loop {
        match select(COMMANDS.receive(), Timer::after(LIVE_NOTIFY_INTERVAL)).await {
            Either::First(command) => {
                let action = match &command {
                    Command::Control(payload) => protocol.on_control(payload, now_ms()),
                    Command::Map(payload) => protocol.on_map_write(payload, now_ms()),
                };
                match action {
                    Action::Notify(resp) => notify_control(server, conn, resp),
                    Action::NotifyThenReset(resp) => {
                        notify_control(server, conn, resp);
                        Timer::after(RESET_GRACE).await;
                        return;
                    }
                    // finish_work will carry the deferred ExitAck.
                    Action::ExitDeferred => {}
                    Action::StartWork(kind) => {
                        let flash_ok = match kind {
                            // Complete for a preview means "the LiveOutput
                            // producer observes the candidate": finish_work
                            // installs it before the notification below, and
                            // this same loop computes LiveOutput after.
                            WorkKind::Preview => true,
                            WorkKind::Commit | WorkKind::ResetDefaults => {
                                match protocol.working_map().copied() {
                                    Some(map) => {
                                        crate::ble::prefs::save_prefs(flash, prefs.profile_id, &map)
                                            .await
                                            .is_ok()
                                    }
                                    None => false,
                                }
                            }
                        };
                        if let Some(done) = protocol.finish_work(flash_ok) {
                            let _ = server
                                .config
                                .stored_map_set(&protocol.stored_map().to_bytes());
                            notify_control(server, conn, done.complete);
                            // The active map may just have changed; refresh
                            // LiveOutput without waiting for stick motion.
                            live_seq = live_seq.wrapping_add(1);
                            let (report, _) = current.to_gamepad_report_with(
                                protocol.active_map(),
                                &mut chord,
                                now_ms(),
                            );
                            let _ = server
                                .config
                                .live_output_notify(conn, &encode_live_output(&report, live_seq));
                            if let Some(exit_ack) = done.exit_ack {
                                notify_control(server, conn, exit_ack);
                                Timer::after(RESET_GRACE).await;
                                return;
                            }
                        }
                    }
                }
                let _ = server
                    .config
                    .info_set(&protocol.info(profile_byte, prefs.source));
            }
            Either::Second(()) => {
                if let Some(state) = RAW_CONTROLLER_STATE.try_take() {
                    current = state;
                    live_seq = live_seq.wrapping_add(1);
                    let _ = server
                        .config
                        .live_input_notify(conn, &encode_live_input(&current, live_seq));
                    let (report, _) =
                        current.to_gamepad_report_with(protocol.active_map(), &mut chord, now_ms());
                    let _ = server
                        .config
                        .live_output_notify(conn, &encode_live_output(&report, live_seq));
                }
                match protocol.poll(now_ms()) {
                    Some(Expiry::ArmTimeout(resp)) => {
                        notify_control(server, conn, resp);
                        let _ = server
                            .config
                            .info_set(&protocol.info(profile_byte, prefs.source));
                    }
                    // The engine is between operations here by construction
                    // (work runs inline above), so nothing is abandoned.
                    Some(Expiry::SessionEnd) => return,
                    None => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_address_is_distinct_deterministic_and_random_static() {
        let normal = Address::new(AddressType::Public, [1, 2, 3, 4, 5, 6]);
        let first = derive_config_address(normal);
        let second = derive_config_address(normal);
        assert_eq!(first, second);
        assert_ne!(first, normal);
        assert_eq!(first.address_type() as u8, AddressType::RandomStatic as u8);
        assert_eq!(first.bytes()[5] & 0xC0, 0xC0);
    }
}
