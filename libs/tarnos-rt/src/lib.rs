//! Minimal usermode runtime for TarnOS-native binaries — the "crt0"
//! analog every future TarnOS binary reuses: an entry point, a panic
//! handler, and syscall wrappers. No libc, no host `std`; every
//! TarnOS-native binary is `#![no_std]` and statically linked.
#![no_std]

pub mod syscall;

use core::panic::PanicInfo;

/// Generates the binary's `_start` entry point, which calls `$path`
/// (expected never to return) after the process's initial register/stack
/// state — set up by the kernel's ELF loader — is already valid Rust
/// calling-convention state, needing no further crt0 setup of its own.
#[macro_export]
macro_rules! entry_point {
    ($path:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn _start() -> ! {
            let main: fn() -> ! = $path;
            main()
        }
    };
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // No console access without a capability naming one, and this
    // runtime doesn't assume any particular slot holds one — so a panic
    // just exits with a fixed, recognizable non-zero code rather than
    // trying to print anything.
    syscall::sys_exit(101)
}
