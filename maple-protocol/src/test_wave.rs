// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Test-only waveform builder shared by the block decoder's tests and the
//! packed-capture tests: synthetic byte-wide captures of a VMU `BLOCK_READ`
//! reply, at a variable samples-per-phase, with noise on the six non-Maple
//! pins. Moved out of `block_decode`'s test module for unchanged.

extern crate std;
use std::vec;
use std::vec::Vec;

// pulsarv1's pins: A = P0.02, B = P0.03. Other bits carry noise below.
pub const A: u8 = 1 << 2;
pub const B: u8 = 1 << 3;

/// Deterministic xorshift, so failures reproduce.
pub struct Rng(pub u32);
impl Rng {
    pub fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() as usize) % (hi - lo + 1)
    }
}

/// A waveform builder with a variable hold per state, and noise on the
/// six pins that are not Maple (the sync LED is one of them on pulsarv1).
pub struct Wave {
    pub s: Vec<u8>,
    a: bool,
    b: bool,
    rng: Rng,
    lo: usize,
    hi: usize,
}

impl Wave {
    pub fn new(seed: u32, lo: usize, hi: usize) -> Self {
        Self {
            s: Vec::new(),
            a: true,
            b: true,
            rng: Rng(seed),
            lo,
            hi,
        }
    }
    pub fn push(&mut self, n: usize) {
        for _ in 0..n {
            let noise = self.rng.next().to_le_bytes()[0] & !(A | B);
            self.s
                .push(noise | if self.a { A } else { 0 } | if self.b { B } else { 0 });
        }
    }
    pub fn hold(&mut self) {
        let n = self.rng.range(self.lo, self.hi);
        self.push(n);
    }
    pub fn set(&mut self, a: bool, b: bool) {
        self.a = a;
        self.b = b;
        self.hold();
    }
    pub fn start_pattern(&mut self) {
        self.set(true, true);
        self.push(20);
        self.set(false, true);
        for _ in 0..3 {
            self.set(false, false);
            self.set(false, true);
        }
        self.set(false, false);
        self.set(false, true);
        self.set(true, true);
        self.set(true, false);
    }
    pub fn bit(&mut self, bit: bool, a_is_clock: bool) {
        if a_is_clock {
            self.set(self.a, bit);
            self.set(false, self.b);
            self.set(self.a, true);
        } else {
            self.set(bit, self.b);
            self.set(self.a, false);
            self.set(true, self.b);
        }
    }
    /// A frame as the VMU sends it: 16-byte chunks with a pause after
    /// each (the bus sits at A high, B low between chunks).
    pub fn frame(&mut self, bytes: &[u8], pause: (usize, usize)) {
        self.start_pattern();
        let mut a_is_clock = true;
        for (i, &byte) in bytes.iter().enumerate() {
            for k in (0..8).rev() {
                self.bit(byte >> k & 1 != 0, a_is_clock);
                a_is_clock = !a_is_clock;
            }
            if i % 16 == 15 && i + 1 < bytes.len() {
                let n = self.rng.range(pause.0, pause.1);
                self.push(n);
            }
        }
        self.push(4000);
    }
}

pub fn block_frame(block: u8, seed: u32) -> Vec<u8> {
    let mut rng = Rng(seed);
    let mut f = vec![130u8, 0x01, 0x00, 0x08];
    f.extend_from_slice(&[2, 0, 0, 0, block, 0, 0, 0]);
    for _ in 0..512 {
        f.push(rng.next().to_le_bytes()[0]);
    }
    let x = f.iter().fold(0u8, |c, &b| c ^ b);
    f.push(x);
    f
}
