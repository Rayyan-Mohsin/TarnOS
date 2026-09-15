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

    // Must run before anything else below, including this core's own
    // diagnostic print: a ring-0 panic means the whole machine is
    // already fatally broken, and this core may be holding literally
    // any lock (`earlycon::COM1_TX_LOCK`, `task::executor::EXECUTOR`,
    // `task::scheduler::SCHEDULER`, ...) at the exact instant its fault
    // struck -- a hardware fault jumps straight to a new handler without
    // ever running the interrupted frame's `Drop`, so nothing else will
    // ever release it. Without this, every other core that later needs
    // that same lock spins forever, indistinguishable from the outside
    // (no further serial output, high CPU usage that a test runner
    // outside the guest can't see) from a genuine, unrecoverable system
    // hang -- exactly what a live GDB session caught happening here (see
    // `arch::x86_64::lapic::PANIC_HALT_VECTOR`'s doc comment for the full
    // incident). Lock-free and cannot itself get stuck.
    crate::arch::x86_64::lapic::broadcast_panic_halt(core);

    if PANICKING[core].swap(true, Ordering::SeqCst) {
        loop {
            unsafe {
                asm!("cli", "hlt", options(nomem, nostack));
            }
        }
    }

    // `panic_println`, not `earlyprintln!`/`_println`: the broadcast
    // above only protects *other* cores from `COM1_TX_LOCK` being
    // orphaned. This exact core can *also* have orphaned it, on itself,
    // an instant ago -- an ordinary, unrelated `earlyprintln!` call
    // interrupted mid-write by the very fault that led here, its guard
    // never dropped. See `earlycon::panic_println`'s doc comment for the
    // live-GDB-caught incident this closes.
    crate::earlycon::panic_println(format_args!(""));
    if let Some(location) = info.location() {
        crate::earlycon::panic_println(format_args!(
            "[KERNEL PANIC] {}:{}:{}: {}",
            location.file(),
            location.line(),
            location.column(),
            info.message()
        ));
    } else {
        crate::earlycon::panic_println(format_args!("[KERNEL PANIC] {}", info.message()));
    }
    crate::earlycon::panic_println(format_args!("halting."));

    loop {
        unsafe {
            asm!("cli", "hlt", options(nomem, nostack));
        }
    }
}
