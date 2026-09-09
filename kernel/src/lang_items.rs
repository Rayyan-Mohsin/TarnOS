//! `#[panic_handler]` and other lang items the kernel binary must provide
//! since it has no standard library.

use core::arch::asm;
use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    earlyprintln!();
    if let Some(location) = info.location() {
        earlyprintln!(
            "[KERNEL PANIC] {}:{}:{}: {}",
            location.file(),
            location.line(),
            location.column(),
            info.message()
        );
    } else {
        earlyprintln!("[KERNEL PANIC] {}", info.message());
    }
    earlyprintln!("halting.");

    loop {
        unsafe {
            asm!("cli", "hlt", options(nomem, nostack));
        }
    }
}
