// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Dreamcast controller protocol library.
//!
//! Pure logic for Maple Bus packet construction, controller state parsing,
//! and Xbox One S BLE HID report generation. No embedded or hardware
//! dependencies — just `heapless` for `no_std` collections.
//!
//! # Not covered
//!
//! Hardware-specific code lives in the main crate:
//! - `MapleBus` (GPIO register access for Maple Bus signaling)
//! - `MapleHost` (uses `MapleBus` for TX/RX transactions)
//! - `BatteryReader` (SAADC peripheral)
//! - BLE GATT services (nrf-softdevice macros)
//! - Power management (`enter_system_off`, `disable_boost`)

#![no_std]

// which order a VMU block's bytes are in, on the wire and on the
// BLE link. The contract, not a decode step — see the module doc.
pub mod block_bytes;
// the incremental decoder for byte-wide VMU block captures.
pub mod block_decode;
pub mod config_protocol;
pub mod controller_state;
pub mod guide_chord;
// the host-facing VMU storage service — who is owed what, and what
// goes out next. Host-tested here; `ble::host_vmu` is the static it lives in.
pub mod host_vmu_io;
// whether the pad is being used — the gate on serving block reads.
pub mod input_idle;
// when the HID notify loop gives up on a connection. Host-tested
// here; `ble::task` owns the link and acts on the verdict.
pub mod notify_budget;
#[cfg(test)]
mod oracle;
pub mod packed;
pub mod packet;
pub mod prefs_journal;
// who owns each slot of the block-read pipeline. Host-tested here
// because the firmware crate's tests do not run; `maple::block_read` drives it.
pub mod read_pipeline;
// the block-read cadence, its LCD-gap
// deferral and the display hold. Host-tested; `maple::block_read` drives it.
pub mod read_sched;
pub mod remap;
pub mod sync_hold;
#[cfg(test)]
mod test_wave;
pub mod wire;
// one block's write onto the VMU: phases, commit, read-back, the retry budget.
// Host-tested; `maple::block_write` drives it.
pub mod write_seq;
pub mod xbox_hid;
