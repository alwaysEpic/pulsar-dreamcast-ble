// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

// VMU storage block reads. Needs the hardware capture — a 53,248
// sample window is exactly what route (a) replaced the CPU sampler for — so it
// is absent on the DK, which alone still samples on the CPU (ADR-018).
#[cfg(feature = "spim-capture")]
pub mod block_read;
// VMU storage block writes: four phases, the commit, and a read-back through
// `block_read` — so present exactly where it is.
#[cfg(feature = "spim-capture")]
pub mod block_write;
pub mod controller_state;
pub mod gpio_bus;
pub mod host;
pub mod packet;
pub mod pwm_tx;
pub mod radio_notify;
pub mod spim_capture;
pub mod timeslot_tx;

pub use controller_state::ControllerState;
pub use gpio_bus::MapleBus;
pub use host::MapleHost;
pub use packet::MaplePacket;
