//! Bare, direct-port-I/O debug console.
//!
//! Used for boot diagnostics and fault handlers before the real UART driver
//! (`driver::uart`, with proper 16550 initialization and LSR busy-checking)
//! exists. QEMU's 16550 emulation accepts bytes on THR (0x3F8) with no
//! prior setup, which is sufficient for this early, single-threaded use.
use core::arch::asm;
use core::fmt::{self, Write};

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
    let _ = EarlyCon.write_fmt(args);
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
    ($($arg:tt)*) => {{
        $crate::earlycon::_print(format_args!($($arg)*));
        $crate::earlyprint!("\r\n");
    }};
}
