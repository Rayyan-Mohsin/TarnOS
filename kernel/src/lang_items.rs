//! `#[panic_handler]` and other lang items the kernel binary must provide
//! since it has no standard library.

use core::arch::asm;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

/// The machine-wide "who gets to report this panic" election. Holds
/// `usize::MAX` until the first core claims it via
/// [`claim_panic_reporter`], after which it holds that core's index
/// forever (this machine never recovers from a panic, so it never needs
/// to be reset).
static PANIC_REPORTER_CORE: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Elects exactly one core, machine-wide, as the sole panic reporter,
/// and halts every *other* core immediately, before it can print
/// anything at all.
///
/// This closes a race `broadcast_panic_halt` alone does not:
/// `task::scheduler::dump_cores_for_panic` is called directly from
/// `idt.rs`'s ring0 fault handlers *before* the eventual `panic!()`
/// ever reaches this module's own `broadcast_panic_halt` call. If two
/// cores fault within a few instructions of each other -- exactly what
/// a shared-state corruption bug tends to cause -- both can reach a
/// diagnostic print before either one's halt IPI is actually serviced
/// by the other, interleaving their output byte-for-byte on the one
/// physical UART (`earlycon::panic_println`'s bounded spin-then-
/// `break_lock` fallback makes this worse, not better: under this
/// emulator, a tight spin loop can complete *faster* than the handful
/// of slow, VM-exiting `out dx, al` port writes the other core's
/// in-progress line still needs, so the "safety" spin can force the
/// lock open on a write that is still genuinely, legitimately
/// in-progress). Caught live: `run_12.log` of a `switch_to` hardening
/// stress batch shows exactly this -- two interleaved
/// `[panic-dump] core 0: ...` lines, the first cut off mid-write,
/// immediately ahead of the run's real, load-bearing evidence (a
/// `scheduler.rs:574` assertion failure) getting truncated the same
/// way before its message could finish printing.
///
/// Every caller -- both `idt.rs`'s handlers (ahead of their own
/// `dump_cores_for_panic` call) and this module's own `panic` (ahead of
/// its `broadcast_panic_halt` and diagnostic print) -- must call this
/// as the very first thing they do. Returns `true` to whichever core
/// wins the race (and to every later call from that *same* core, so a
/// fault's `idt.rs` handler and the `panic!()` it eventually raises
/// both see themselves as the reporter); every other core halts here
/// and never returns.
pub fn claim_panic_reporter(this_core: usize) -> bool {
    match PANIC_REPORTER_CORE.compare_exchange(
        usize::MAX,
        this_core,
        Ordering::SeqCst,
        Ordering::SeqCst,
    ) {
        Ok(_) => {
            crate::arch::x86_64::lapic::broadcast_panic_halt(this_core);
            true
        }
        Err(winner) if winner == this_core => true,
        Err(_) => loop {
            unsafe {
                asm!("cli", "hlt", options(nomem, nostack));
            }
        },
    }
}

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
    // incident). `claim_panic_reporter` both sends this broadcast (on the
    // winning core only) and, on every *other* core, halts before it can
    // print anything itself -- see that function's own doc comment for
    // the concurrent-panic UART-interleaving incident this closes.
    // Lock-free and cannot itself get stuck.
    claim_panic_reporter(core);

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
