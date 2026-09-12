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
use core::arch::asm;
use core::fmt::{self, Write};

use crate::sync::SpinLock;

static EARLYCON_LOCK: SpinLock<()> = SpinLock::new(());

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
    let _guard = EARLYCON_LOCK.lock();
    let _ = EarlyCon.write_fmt(args);
}

/// Like [`_print`], but writes a trailing `"\r\n"` under the same lock
/// guard as the message itself — see this module's doc comment.
#[doc(hidden)]
pub fn _println(args: fmt::Arguments) {
    let _guard = EARLYCON_LOCK.lock();
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
