// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Panic handler that logs to flash before resetting.
//!
//! Writes a magic word + truncated panic message to a dedicated flash page
//! (`0xFC000`) using raw NVMC register writes (no `SoftDevice` or async).
//! On boot, call [`check_panic_log`] to print any stored panic via RTT
//! and clear the page.

use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{self, Ordering};

/// Flash page for panic log (one page before name preference at 0xFD000).
const PANIC_FLASH_ADDR: u32 = 0x000F_C000;

/// Magic number to identify valid panic data.
const PANIC_MAGIC: u32 = 0xDEAD_BEEF;

/// Max bytes for panic message (leaving 4 bytes for magic).
const MAX_MSG_LEN: usize = 252;

/// NVMC register addresses (nRF52840).
const NVMC_BASE: u32 = 0x4001_E000;
const NVMC_READY: *const u32 = (NVMC_BASE + 0x400) as *const u32;
const NVMC_CONFIG: *mut u32 = (NVMC_BASE + 0x504) as *mut u32;
const NVMC_ERASEPAGE: *mut u32 = (NVMC_BASE + 0x508) as *mut u32;

/// Wait for NVMC to be ready.
#[inline]
fn nvmc_wait() {
    // SAFETY: Reading a hardware register.
    while unsafe { core::ptr::read_volatile(NVMC_READY) } == 0 {}
}

/// Erase one flash page using raw NVMC registers.
///
/// # Safety
/// Caller must ensure the page address is valid and not in use by `SoftDevice`.
unsafe fn nvmc_erase_page(addr: u32) {
    nvmc_wait();
    core::ptr::write_volatile(NVMC_CONFIG, 2); // Erase enable
    nvmc_wait();
    core::ptr::write_volatile(NVMC_ERASEPAGE, addr);
    nvmc_wait();
    core::ptr::write_volatile(NVMC_CONFIG, 0); // Read-only
    nvmc_wait();
}

/// Write a 4-byte aligned word to flash using raw NVMC registers.
///
/// # Safety
/// Caller must ensure the address is valid, aligned, and in an erased page.
unsafe fn nvmc_write_word(addr: u32, value: u32) {
    nvmc_wait();
    core::ptr::write_volatile(NVMC_CONFIG, 1); // Write enable
    nvmc_wait();
    core::ptr::write_volatile(addr as *mut u32, value);
    nvmc_wait();
    core::ptr::write_volatile(NVMC_CONFIG, 0); // Read-only
    nvmc_wait();
}

/// Small fixed-capacity buffer for formatting the panic message.
struct PanicBuf {
    buf: [u8; MAX_MSG_LEN],
    pos: usize,
}

impl PanicBuf {
    const fn new() -> Self {
        Self {
            buf: [0u8; MAX_MSG_LEN],
            pos: 0,
        }
    }
}

impl Write for PanicBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = MAX_MSG_LEN - self.pos;
        let len = bytes.len().min(remaining);
        self.buf[self.pos..self.pos + len].copy_from_slice(&bytes[..len]);
        self.pos += len;
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Format the panic message into a stack buffer
    let mut buf = PanicBuf::new();
    let _ = write!(buf, "{info}");

    // SAFETY: Writing to a dedicated flash page that is not used by SoftDevice.
    // In a panic handler, nothing else is running, so NVMC access is safe.
    unsafe {
        nvmc_erase_page(PANIC_FLASH_ADDR);

        // Write magic word
        nvmc_write_word(PANIC_FLASH_ADDR, PANIC_MAGIC);

        // Write message in 4-byte words (flash requires word-aligned writes)
        let words = buf.pos.div_ceil(4);
        for i in 0..words {
            let offset = i * 4;
            let mut word_bytes = [0u8; 4];
            for (j, byte) in word_bytes.iter_mut().enumerate() {
                if offset + j < buf.pos {
                    *byte = buf.buf[offset + j];
                }
            }
            let word = u32::from_le_bytes(word_bytes);
            #[allow(clippy::cast_possible_truncation)]
            nvmc_write_word(PANIC_FLASH_ADDR + 4 + (i as u32) * 4, word);
        }
    }

    cortex_m::peripheral::SCB::sys_reset();
}

/// Check for a stored panic log and print it via RTT, then clear the page.
///
/// Call once at early boot, after RTT is initialized.
pub fn check_panic_log() {
    // SAFETY: Reading from flash at a known address.
    let magic = unsafe { core::ptr::read_volatile(PANIC_FLASH_ADDR as *const u32) };

    if magic != PANIC_MAGIC {
        return;
    }

    crate::log!("=== PANIC LOG (from previous run) ===");

    // Read message bytes until null or end of buffer
    let msg_base = (PANIC_FLASH_ADDR + 4) as *const u8;
    let mut len = 0;
    while len < MAX_MSG_LEN {
        // SAFETY: Reading from flash within the page.
        let byte = unsafe { core::ptr::read_volatile(msg_base.add(len)) };
        if byte == 0 || byte == 0xFF {
            break;
        }
        len += 1;
    }

    if len > 0 {
        let msg_slice = unsafe { core::slice::from_raw_parts(msg_base, len) };
        if let Ok(msg) = core::str::from_utf8(msg_slice) {
            crate::log!("{}", msg);
        } else {
            crate::log!("(panic message was not valid UTF-8)");
        }
    }

    crate::log!("=== END PANIC LOG ===");

    // Clear the page so we don't print the same panic every boot.
    // Use NVMC directly since SoftDevice may not be initialized yet.
    unsafe {
        nvmc_erase_page(PANIC_FLASH_ADDR);
    }

    // Brief visual indication that a panic was recovered
    // (don't block long — just enough to notice on debugger)
    for _ in 0..100_000 {
        atomic::compiler_fence(Ordering::SeqCst);
    }
}
