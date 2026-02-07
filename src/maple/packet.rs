#![allow(dead_code)] // Some methods for future use

use heapless::Vec;

/// Represents a Maple Bus packet ready to send or parse.
pub struct MaplePacket {
    pub sender: u8,
    pub recipient: u8,
    pub command: u8,
    pub payload: Vec<u32, 255>, // up to 255 u32 words
}

impl Default for MaplePacket {
    fn default() -> Self {
        Self {
            sender: 0,
            recipient: 0,
            command: 0,
            payload: Vec::new(),
        }
    }
}

impl MaplePacket {
    /// Builds the 32-bit frame word used at the start of the packet.
    /// Format: [length:8][sender:8][recipient:8][command:8]
    /// Byte 0 = length, Byte 1 = sender, Byte 2 = recipient, Byte 3 = command
    pub fn frame_word(&self) -> u32 {
        let num_words = self.payload.len() as u32;
        ((self.command as u32) << 24)      // Byte 3 = command
            | ((self.recipient as u32) << 16) // Byte 2 = recipient
            | ((self.sender as u32) << 8)     // Byte 1 = sender
            | (num_words & 0xFF) // Byte 0 = length
    }

    /// Computes the CRC over the frame and payload.
    pub fn crc(&self) -> u8 {
        let mut crc = 0u8;
        let frame = self.frame_word();
        Self::crc8_word(frame, &mut crc);
        for word in self.payload.iter() {
            Self::crc8_word(*word, &mut crc);
        }
        crc
    }

    fn crc8_word(word: u32, crc: &mut u8) {
        for i in 0..4 {
            *crc ^= ((word >> (i * 8)) & 0xFF) as u8;
        }
    }

    /// Appends serialized words to the buffer (including frame and CRC)
    pub fn encode(&self, buffer: &mut heapless::Vec<u32, 258>) {
        buffer.clear();
        buffer.push(self.frame_word()).unwrap();
        buffer.extend_from_slice(&self.payload).unwrap();
        buffer.push(self.crc() as u32).unwrap();
    }

    /// Attempts to parse a received packet from words.
    pub fn decode(words: &[u32]) -> Option<Self> {
        if words.len() < 2 {
            return None;
        }

        let frame = words[0];
        let crc_received = (words.last()? & 0xFF) as u8;

        let mut crc_calc = 0u8;
        Self::crc8_word(frame, &mut crc_calc);
        for word in &words[1..words.len() - 1] {
            Self::crc8_word(*word, &mut crc_calc);
        }

        if crc_calc != crc_received {
            // corruppted packet
            return None;
        }

        let command = ((frame >> 24) & 0xFF) as u8; // Byte 3
        let recipient = ((frame >> 16) & 0xFF) as u8; // Byte 2
        let sender = ((frame >> 8) & 0xFF) as u8; // Byte 1
        let length = (frame & 0xFF) as usize; // Byte 0

        if words.len() < length + 2 {
            return None;
        }

        let mut payload = heapless::Vec::new();
        payload.extend_from_slice(&words[1..1 + length]).ok()?;

        Some(Self {
            sender,
            recipient,
            command,
            payload,
        })
    }
}
