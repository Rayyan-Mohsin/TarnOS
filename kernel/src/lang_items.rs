//! `#[panic_handler]` and other lang items the kernel binary must provide
//! since it has no standard library.

use core::arch::asm;
use core::panic::PanicInfo;

fn panic_print(s: &str) {
    for byte in s.bytes() {
        unsafe {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nomem, nostack, preserves_flags));
        }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    panic_print("\r\n[KERNEL PANIC] ");
    if let Some(location) = info.location() {
        panic_print(location.file());
        panic_print(":");
    }
    // PanicInfo's message doesn't implement a no_std-friendly formatter here
    // without pulling in `core::fmt::Write` plumbing; that arrives with the
    // real console driver. For now the location alone is enough to prove
    // panics don't triple-fault the machine.
    panic_print(" kernel panicked, halting.\r\n");

    loop {
        unsafe {
            asm!("cli; hlt", options(nomem, nostack));
        }
    }
}
