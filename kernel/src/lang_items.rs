//! `#[panic_handler]` and other lang items the kernel binary must provide
//! since it has no standard library.

use core::arch::asm;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

/// A per-core panic reentrancy guard. Without this, a *second* fault
/// landing on the same core while this handler is still mid-print
/// (still holding `earlycon::COM1_TX_LOCK` on its own call stack)
/// re-enters this same function, whose own first `earlyprintln!()` then
/// deadlocks trying to re-lock a `spin::Mutex`-backed lock this exact
/// core already holds -- turning what should be a diagnosable panic
/// message into an unrecoverable, silent hang instead. Found via
/// `xtask test-smp-kill-cross-core` stress-testing this milestone's new
/// per-core forced preemption timer (see `docs/adr/0011`): a second,
/// genuine hardware fault occasionally strikes while an *earlier* one is
/// still being reported, and skipping straight to halting on that
/// second fault -- rather than trying (and deadlocking) to print it too
/// -- at least gets the first fault's own message out.
static PANICKING: [AtomicBool; crate::arch::x86_64::percpu::MAX_CORES] =
    [const { AtomicBool::new(false) }; crate::arch::x86_64::percpu::MAX_CORES];

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let core = crate::arch::x86_64::percpu::core_index();
    if PANICKING[core].swap(true, Ordering::SeqCst) {
        loop {
            unsafe {
                asm!("cli", "hlt", options(nomem, nostack));
            }
        }
    }

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
