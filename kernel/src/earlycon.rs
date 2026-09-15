//! Bare, direct-port-I/O debug console.
//!
//! Used for boot diagnostics and fault handlers before the real UART driver
//! (`driver::uart`, with proper 16550 initialization and LSR busy-checking)
//! exists. QEMU's 16550 emulation accepts bytes on THR (0x3F8) with no
//! prior setup.
//!
//! `SpinLock`, not a plain `spin::Mutex`: fault handlers call
//! `earlyprintln!` directly from interrupt/exception context, and now that
//! more than one core can exist, two cores logging at the same time would
//! otherwise interleave their writes byte-by-byte, corrupting the log a
//! test parses. [`_println`] holds the lock across both the formatted
//! message and its trailing `"\r\n"` (one lock/unlock, not two separate
//! ones), so a full line from one core is never split by another core's
//! output landing in the middle of it.
//!
//! [`COM1_TX_LOCK`] is `pub(crate)`, not private to this module: this bare
//! poke and `driver::uart::Uart16550`'s real, LSR-checked writes both
//! target the exact same physical transmit register (0x3F8) but started
//! out under two entirely separate locks, each correctly serializing
//! writers *within* its own path while doing nothing to stop the two
//! paths from interleaving with *each other*. A real, reproduced bug
//! (`xtask test-smp-forced-preempt`, intermittently: a boot-time
//! `earlyprintln!` call on the BSP racing a process's own IPC message
//! being relayed through `console_server` -> `driver::uart::write_bytes`
//! on a different core, producing a visibly garbled log line) — see
//! `docs/adr/0011`. `driver::uart` locks this same shared lock around its
//! own transmit calls now, so every writer to the physical wire is
//! mutually exclusive regardless of which logical driver initiated it.
use core::arch::asm;
use core::fmt::{self, Write};

use crate::sync::SpinLock;

pub(crate) static COM1_TX_LOCK: SpinLock<()> = SpinLock::new(());

pub struct EarlyCon;

impl Write for EarlyCon {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            unsafe {
                asm!(
                    "out dx, al",
                    in("dx") 0x3F8u16,
                    in("al") byte,
                    options(nomem, nostack, preserves_flags)
                );
            }
        }
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let _guard = COM1_TX_LOCK.lock();
    let _ = EarlyCon.write_fmt(args);
}

/// Like [`_print`], but writes a trailing `"\r\n"` under the same lock
/// guard as the message itself — see this module's doc comment.
#[doc(hidden)]
pub fn _println(args: fmt::Arguments) {
    let _guard = COM1_TX_LOCK.lock();
    let _ = EarlyCon.write_fmt(args);
    let _ = EarlyCon.write_str("\r\n");
}

/// `lang_items::panic`'s own print, in place of [`_println`]: bounded
/// spin, then forces `COM1_TX_LOCK` open rather than waiting forever.
///
/// `arch::x86_64::lapic::broadcast_panic_halt` (which `panic` calls
/// before this) stops every *other* core from ever contending for this
/// lock again, but cannot help against this *same* core still, in
/// effect, holding it -- caught live via GDB: a page fault struck this
/// exact core mid-write of an ordinary, unrelated `earlyprintln!` call,
/// jumping straight to `idt::page_fault_ring0` without ever running the
/// interrupted frame's `Drop`, so its `COM1_TX_LOCK` guard never
/// released. `panic`'s own first `earlyprintln!` then hung here forever
/// -- indistinguishable, from the serial log alone, from the machine
/// silently dying before printing anything at all. A bounded spin still
/// prefers the correct, non-interleaved outcome when this really is
/// just ordinary cross-core contention (the common case), and only
/// forces the issue once that stops being plausible.
pub fn panic_println(args: fmt::Arguments) {
    const MAX_SPINS: u64 = 10_000_000;
    let mut guard = COM1_TX_LOCK.try_lock();
    let mut spins = 0u64;
    while guard.is_none() && spins < MAX_SPINS {
        core::hint::spin_loop();
        spins += 1;
        guard = COM1_TX_LOCK.try_lock();
    }
    if guard.is_none() {
        // SAFETY: see `SpinLock::break_lock`'s doc comment -- this is
        // exactly its one sanctioned caller. Correctness from here on
        // depends only on `EarlyCon`'s raw port write, never on
        // `COM1_TX_LOCK` genuinely excluding a still-live writer; the
        // worst case is an interleaved, still-diagnosable line instead
        // of silence forever.
        unsafe { COM1_TX_LOCK.break_lock() };
        guard = COM1_TX_LOCK.try_lock();
    }
    let _guard = guard;
    let _ = EarlyCon.write_fmt(args);
    let _ = EarlyCon.write_str("\r\n");
}

#[macro_export]
macro_rules! earlyprint {
    ($($arg:tt)*) => {
        $crate::earlycon::_print(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! earlyprintln {
    () => {
        $crate::earlyprint!("\r\n")
    };
    ($($arg:tt)*) => {
        $crate::earlycon::_println(format_args!($($arg)*))
    };
}
