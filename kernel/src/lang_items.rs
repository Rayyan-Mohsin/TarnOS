//! `#[panic_handler]` and other lang items the kernel binary must provide
//! since it has no standard library.

use core::arch::asm;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

// TEMPORARY debugging aid, alongside the flight recorder in
// `task::scheduler`: a per-core reentrancy guard. Without this, a
// *second* fault landing on the same core while this handler is still
// mid-print (still holding `earlycon::EARLYCON_LOCK` on its own call
// stack) re-enters this same function, whose own first `earlyprintln!()`
// then deadlocks trying to re-lock a `spin::Mutex`-backed lock this
// exact core already holds -- explaining an observed hang with a
// truncated panic line rather than a second, distinct message. Skip
// straight to halting on a nested panic instead, so the *first* panic's
// own message and flight-recorder dump have a chance to actually finish.
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
    crate::task::scheduler::dump_flight_recorder();
    earlyprintln!("halting.");

    loop {
        unsafe {
            asm!("cli", "hlt", options(nomem, nostack));
        }
    }
}
