// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Panic handler that logs to flash before resetting.
//!
//! Writes a magic word + truncated panic message to a dedicated flash page
//! (`0xF1000`) using raw NVMC register writes (no `SoftDevice` or async).
//! On boot, call [`check_panic_log`] to print any stored panic via RTT
//! and clear the page.

use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{self, Ordering};

/// Flash page for panic log — bottom page of the app-data window
/// (`0xF1000-0xF3FFF`, one page below the name preference at `0xF2000`).
/// Moved 2026-08-04 from `0xFC000`, which lay inside the bootloader region.
const PANIC_FLASH_ADDR: u32 = 0x000F_1000;

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
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "one NVMC erase transaction: enable, erase, back to read-only, each \
                  separated by a ready-wait. The sequence is the safety argument"
    )]
    // SAFETY: `NVMC_CONFIG` and `NVMC_ERASEPAGE` are fixed, word-aligned
    // registers. `addr` is the caller's responsibility per the `# Safety`
    // contract above. `nvmc_wait` spins until the controller reports ready
    // before and after each step, which is what NVMC requires between mode
    // changes; CONFIG is returned to read-only so no stray write can reach
    // flash afterwards.
    unsafe {
        nvmc_wait();
        core::ptr::write_volatile(NVMC_CONFIG, 2); // Erase enable
        nvmc_wait();
        core::ptr::write_volatile(NVMC_ERASEPAGE, addr);
        nvmc_wait();
        core::ptr::write_volatile(NVMC_CONFIG, 0); // Read-only
        nvmc_wait();
    }
}

/// Write a 4-byte aligned word to flash using raw NVMC registers.
///
/// # Safety
/// Caller must ensure the address is valid, aligned, and in an erased page.
unsafe fn nvmc_write_word(addr: u32, value: u32) {
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "one NVMC write transaction: enable, store, back to read-only, each \
                  separated by a ready-wait. The sequence is the safety argument"
    )]
    // SAFETY: `NVMC_CONFIG` is a fixed, word-aligned register. That `addr` is
    // valid, word-aligned and inside an erased page is the caller's
    // responsibility per the `# Safety` contract above. `nvmc_wait` spins until
    // the controller reports ready around each step, and CONFIG is returned to
    // read-only so no stray write can reach flash afterwards.
    unsafe {
        nvmc_wait();
        core::ptr::write_volatile(NVMC_CONFIG, 1); // Write enable
        nvmc_wait();
        core::ptr::write_volatile(addr as *mut u32, value);
        nvmc_wait();
        core::ptr::write_volatile(NVMC_CONFIG, 0); // Read-only
        nvmc_wait();
    }
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

    // Only ever store the FIRST panic. A panic that reboots straight back into
    // the same panic would otherwise erase and rewrite this page every cycle —
    // an ~85ms NVMC page erase every couple of seconds, indefinitely.
    //
    // That is not a theoretical cost. A power transition during an NVMC erase
    // (unplugging USB, where the rail hands from VIN to the IP5306 boost and
    // dips) can corrupt flash well beyond the target page, and losing the
    // bootloader turns a one-line bug into a module that needs SWD to revive.
    // A reset loop must be survivable: you must always be able to double-tap
    // into the bootloader. Bounding flash writes to one is what buys that.
    //
    // SAFETY: reading a known flash address; no other code is running.
    let already_stored =
        unsafe { core::ptr::read_volatile(PANIC_FLASH_ADDR as *const u32) } == PANIC_MAGIC;

    if !already_stored {
        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "erase-then-write is one flash transaction and the ordering is \
                      the safety argument; splitting it would separate operations \
                      that are only sound as a sequence"
        )]
        // SAFETY: `PANIC_FLASH_ADDR` names a page reserved for this handler in
        // memory.x and claimed by no other code — not the SoftDevice, not the
        // bond store, not the bootloader settings — so erasing and rewriting it
        // cannot corrupt anything else. The erase strictly precedes the writes,
        // which is what NVMC requires (a flash word can only be written once per
        // erase). We are inside the panic handler with interrupts effectively
        // dead and nothing else running, so no concurrent NVMC user can race the
        // controller. Every write stays inside the page: `words` is bounded by
        // `buf.pos <= MAX_MSG_LEN` and the page holds the magic plus that
        // message.
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
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "i is bounded by words = buf.pos.div_ceil(4), and buf.pos \
                              is at most MAX_MSG_LEN — far inside u32"
                )]
                nvmc_write_word(PANIC_FLASH_ADDR + 4 + (i as u32) * 4, word);
            }
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
        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "the offset and the read are a single indivisible access; the \
                      bound proved above is what makes both sound"
        )]
        // SAFETY: `len < MAX_MSG_LEN` is the loop condition, and the reserved
        // page holds the magic word plus MAX_MSG_LEN message bytes, so
        // `msg_base.add(len)` stays inside the same allocation the pointer was
        // derived from. Flash is readable as bytes at any alignment, and the
        // page is always initialised — an erased cell reads 0xFF, which the
        // check below treats as end-of-message.
        let byte = unsafe { core::ptr::read_volatile(msg_base.add(len)) };
        if byte == 0 || byte == 0xFF {
            break;
        }
        len += 1;
    }

    if len > 0 {
        // SAFETY: the loop above established that `msg_base .. msg_base + len`
        // are all readable bytes within the reserved page, and `len <=
        // MAX_MSG_LEN`. The region is flash: it is immutable for the lifetime of
        // this borrow (nothing writes the page until the clear below, which
        // happens after `msg_slice` is dropped), so the aliasing rules for a
        // shared slice hold. `u8` has no alignment requirement.
        let msg_slice = unsafe { core::slice::from_raw_parts(msg_base, len) };
        if let Ok(_msg) = core::str::from_utf8(msg_slice) {
            crate::log!("{}", _msg);
        } else {
            crate::log!("(panic message was not valid UTF-8)");
        }
    }

    crate::log!("=== END PANIC LOG ===");

    // Clear the page so we don't print the same panic every boot — but ONLY on
    // `rtt` builds, where someone is attached to have actually read it.
    //
    // On a production build `log!` compiles to nothing, so the message above was
    // never displayed and erasing here would only re-arm the panic handler to
    // write again next time. A reset loop would then do *two* ~85ms page erases
    // per cycle, forever, with a power transition during any one of them able to
    // take out the bootloader. Leaving the page dirty costs nothing (it is
    // unreadable without RTT anyway) and keeps a loop flash-silent.
    #[cfg(feature = "rtt")]
    // SAFETY: `PANIC_FLASH_ADDR` names the page reserved for this handler in
    // memory.x, claimed by no other code, so erasing it cannot disturb the
    // SoftDevice, the bond store or the bootloader settings. This runs at boot
    // from `check_panic_log`, before any task is spawned, so nothing else can be
    // using the NVMC controller concurrently.
    unsafe {
        nvmc_erase_page(PANIC_FLASH_ADDR);
    }

    // Brief visual indication that a panic was recovered
    // (don't block long — just enough to notice on debugger)
    for _ in 0..100_000 {
        atomic::compiler_fence(Ordering::SeqCst);
    }
}
