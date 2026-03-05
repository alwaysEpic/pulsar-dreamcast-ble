// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Board-specific pin mappings and peripherals.
//!
//! Each board module exports:
//! - `BoardPins`: struct with typed pin fields
//! - `init_pins()`: initialize all board-specific pins
//! - `StatusIndicator`: LED control with logical state methods
//! - `PIN_A_BIT` / `PIN_B_BIT`: bit positions for GPIO bus masks

#[cfg(feature = "board-dk")]
mod dk;
#[cfg(feature = "board-xiao")]
mod xiao;

#[cfg(feature = "board-dk")]
pub use dk::*;
#[cfg(feature = "board-xiao")]
pub use xiao::*;
