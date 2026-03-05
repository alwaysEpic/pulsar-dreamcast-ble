// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Maple Bus packet construction.

use heapless::Vec;

/// Represents a Maple Bus packet ready to send or parse.
pub struct MaplePacket {
    pub sender: u8,
    pub recipient: u8,
    pub command: u8,
    pub payload: Vec<u32, 32>, // max 28 words for Device Info, 3 for Get Condition
}

impl MaplePacket {
    /// Builds the 32-bit frame word used at the start of the packet.
    /// Format: `[length:8][sender:8][recipient:8][command:8]`
    /// Byte 0 = length, Byte 1 = sender, Byte 2 = recipient, Byte 3 = command
    #[must_use]
    pub fn frame_word(&self) -> u32 {
        #[allow(clippy::cast_possible_truncation)] // Payload max 255 words
        let num_words = self.payload.len() as u32;
        (u32::from(self.command) << 24)
            | (u32::from(self.recipient) << 16)
            | (u32::from(self.sender) << 8)
            | (num_words & 0xFF)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_word_basic() {
        let packet = MaplePacket {
            sender: 0x00,
            recipient: 0x20,
            command: 0x01,
            payload: Vec::new(),
        };
        // length=0, sender=0x00, recipient=0x20, command=0x01
        // = 0x01_20_00_00
        assert_eq!(packet.frame_word(), 0x0120_0000);
    }

    #[test]
    fn frame_word_with_payload() {
        let mut payload: Vec<u32, 32> = Vec::new();
        payload.push(0x0000_0001).ok();
        let packet = MaplePacket {
            sender: 0x00,
            recipient: 0x20,
            command: 0x09,
            payload,
        };
        // length=1, sender=0x00, recipient=0x20, command=0x09
        // = 0x09_20_00_01
        assert_eq!(packet.frame_word(), 0x0920_0001);
    }
}
