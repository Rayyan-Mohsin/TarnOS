//! Ring-3 entry points for milestone 8's cross-core `SYS_SEND`/`SYS_RECV`
//! and forced-preemption integration tests (see `xtask`'s
//! `test-smp-send-cross-core` and `test-smp-forced-preempt` commands).
//!
//! Same constraints as `milestone7_tests`'s dummy processes: each stays
//! within a single remapped page, every multi-byte value is a `const`
//! baked into the instruction stream, and there are no Rust-level
//! function calls anywhere in these bodies.
#![cfg(any(feature = "smp-send-cross-core-test", feature = "forced-preempt-test"))]
use core::arch::asm;

/// The fixed capability index (in each of these two processes' own,
/// otherwise-empty tables) for the dedicated test endpoint boot code
/// grants them — distinct from `CONSOLE_CAP` (index 0), which both also
/// hold for reporting the result.
#[cfg(feature = "smp-send-cross-core-test")]
const TEST_ENDPOINT_CAP: u64 = 1;

/// `SYS_RECV`s on [`TEST_ENDPOINT_CAP`] immediately — before boot code
/// has spawned [`send_cc_sender_process`] at all, let alone before it
/// could possibly have sent anything — so this always genuinely blocks,
/// registering as a `Waiter::Process` under `Endpoint::slot`'s lock and
/// suspending via `task::scheduler::block_current_process`. This is
/// exactly the race window `Process::pending_wake` closes (see
/// `docs/adr/0011`): once the sender (very likely dispatched to a
/// *different*, previously-idle core — the same `spawn`-order reasoning
/// `milestone7_tests::kill_cc_killer_process`'s doc comment describes)
/// completes the rendezvous, it may call `wake_blocked_process` on this
/// process before this core has actually finished transitioning it to
/// `Blocked`. Reports `SEND_CC_OK`/`SEND_CC_FAIL` on `CONSOLE_CAP`
/// depending on whether the delivered message matches exactly what
/// [`send_cc_sender_process`] sends, then exits.
#[cfg(feature = "smp-send-cross-core-test")]
pub unsafe extern "C" fn send_cc_receiver_process() -> ! {
    const EXPECTED_TAG: u64 = 4;
    const EXPECTED_WORD0: u64 = u64::from_le_bytes(*b"ping\0\0\0\0");
    const WORD0: u64 = u64::from_le_bytes(*b"SEND_CC_");
    const OK_WORD1: u64 = u64::from_le_bytes(*b"OK\0\0\0\0\0\0");
    const FAIL_WORD1: u64 = u64::from_le_bytes(*b"FAIL\0\0\0\0");

    unsafe {
        asm!(
            "mov rax, 2", // SYS_RECV
            "mov rdi, {test_cap}",
            "syscall",
            // rax = 0, rdi = tag, rsi = words[0] on success.
            "cmp rax, 0",
            "jne 2f",
            "cmp rdi, {expected_tag}",
            "jne 2f",
            "cmp rsi, {expected_word0}",
            "jne 2f",

            // Success: report SEND_CC_OK.
            "mov rax, 1", // SYS_SEND
            "mov rdi, 0", // CONSOLE_CAP
            "mov rsi, 10",
            "mov rdx, {word0}",
            "mov r10, {ok_word1}",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "jmp 3f",
            // Failure: report SEND_CC_FAIL.
            "2:",
            "mov rax, 1",
            "mov rdi, 0",
            "mov rsi, 12",
            "mov rdx, {word0}",
            "mov r10, {fail_word1}",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "3:",
            "mov rax, 3", // SYS_EXIT
            "mov rdi, 0",
            "syscall",
            test_cap = const TEST_ENDPOINT_CAP,
            expected_tag = const EXPECTED_TAG,
            expected_word0 = const EXPECTED_WORD0,
            word0 = const WORD0,
            ok_word1 = const OK_WORD1,
            fail_word1 = const FAIL_WORD1,
            options(noreturn, nostack)
        );
    }
}

/// `SYS_SEND`s a fixed message on [`TEST_ENDPOINT_CAP`] immediately upon
/// starting, then exits — no yields, no delay. Boot code spawns
/// [`send_cc_receiver_process`] first specifically so it is very likely
/// to already be blocked in `SYS_RECV` on a different core (a genuinely
/// idle one, notified the instant it was spawned) by the time this one
/// starts and sends — see that function's own doc comment for exactly
/// which race this is meant to hit.
#[cfg(feature = "smp-send-cross-core-test")]
pub unsafe extern "C" fn send_cc_sender_process() -> ! {
    const WORD: u64 = u64::from_le_bytes(*b"ping\0\0\0\0");
    unsafe {
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_SEND,
            in("rdi") TEST_ENDPOINT_CAP,
            in("rsi") 4u64, // tag: byte length of "ping"
            in("rdx") WORD,
            in("r10") 0u64,
            in("r8") 0u64,
            in("r9") 0u64,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_EXIT,
            in("rdi") 0u64,
            options(noreturn)
        );
    }
}

/// Busy-loops forever, making *zero* syscalls at all — unlike
/// `milestone7_tests::kill_cc_target_process` (which cooperatively
/// yields every iteration), nothing here ever traps into the kernel on
/// its own. The only thing that can ever preempt this process is the
/// per-core LAPIC timer landing directly on whatever arbitrary
/// instruction it happens to be executing — the exact scenario every
/// core except the BSP had no way to reach before this milestone. Never
/// exits on its own; `forced_preempt_killer_process` kills it outright.
#[cfg(feature = "forced-preempt-test")]
pub unsafe extern "C" fn forced_preempt_busy_process() -> ! {
    unsafe {
        asm!(
            "2:",
            "pause",
            "jmp 2b",
            options(noreturn, nomem, nostack, preserves_flags)
        );
    }
}

/// A completely ordinary process: reports `PREEMPT_ORD_OK` on
/// `CONSOLE_CAP` and exits immediately. Boot code spawns this after
/// `forced_preempt_busy_process` specifically to confirm an unrelated
/// process still gets to run and exit cleanly despite that other one
/// never yielding the core it landed on.
#[cfg(feature = "forced-preempt-test")]
pub unsafe extern "C" fn forced_preempt_ordinary_process() -> ! {
    const WORD0: u64 = u64::from_le_bytes(*b"PREEMPT_");
    const WORD1: u64 = u64::from_le_bytes(*b"ORD_OK\0\0");
    unsafe {
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_SEND,
            in("rdi") 0u64, // CONSOLE_CAP
            in("rsi") 14u64, // tag: byte length of "PREEMPT_ORD_OK"
            in("rdx") WORD0,
            in("r10") WORD1,
            in("r8") 0u64,
            in("r9") 0u64,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_EXIT,
            in("rdi") 0u64,
            options(noreturn)
        );
    }
}

/// Yields a few times (letting `forced_preempt_busy_process` actually
/// start running, most likely on a different, idle core — the same
/// `spawn`-order reasoning `milestone7_tests::kill_cc_killer_process`'s
/// doc comment describes), then `SYS_KILL`s it — confirming
/// `task::scheduler::terminate_process`'s cross-core eviction protocol
/// (already proven against a *cooperatively*-yielding target by
/// `xtask test-smp-kill-cross-core`) also works against a target that
/// never once traps into the kernel on its own, interrupted by the
/// `RESCHEDULE_VECTOR` IPI at a genuinely arbitrary point in its
/// instruction stream rather than conveniently at a syscall boundary.
/// Immediately afterward, spawns, starts, and waits on one more
/// completely ordinary child to confirm the machine is still fully
/// healthy — same shape as `kill_cc_killer_process`'s own tail. Reports
/// `FRC_PRE_OK`/`FRC_PRE_FAIL` on `CONSOLE_CAP`, then exits.
///
/// Boot code makes this process `forced_preempt_busy_process`'s parent
/// directly and allocates the busy process's `Pid` first, so
/// `TARGET_PID` below (table index 0, generation 1) is the same
/// fixed-value trick `kill_cc_killer_process` already relies on.
#[cfg(feature = "forced-preempt-test")]
pub unsafe extern "C" fn forced_preempt_killer_process() -> ! {
    const TARGET_PID: u64 = 0 | (1u64 << 32);
    const NAME_LO: u64 = u64::from_le_bytes(*b"exit-cod");
    const NAME_HI: u64 = u64::from_le_bytes(*b"e-child\0");
    const WORD0: u64 = u64::from_le_bytes(*b"FRC_PRE_");
    const OK_WORD1: u64 = u64::from_le_bytes(*b"OK\0\0\0\0\0\0");
    const FAIL_WORD1: u64 = u64::from_le_bytes(*b"FAIL\0\0\0\0");

    unsafe {
        asm!(
            // Let the busy process actually get picked up and start
            // running (very likely on a different, previously-idle
            // core) before killing it.
            "mov rax, 0", "syscall",
            "mov rax, 0", "syscall",
            "mov rax, 0", "syscall",
            "mov rax, 0", "syscall",
            "mov rax, 0", "syscall",
            // SYS_KILL(target) -- must succeed against a genuinely
            // Running, never-syscalling, non-cooperative target.
            "mov rax, 8",
            "mov rdi, {target_pid}",
            "syscall",
            "cmp rax, 0",
            "jne 2f", // -> fail

            // Prove the machine is still healthy: a normal spawn+start+wait.
            "mov rax, 4", // SYS_SPAWN("exit-code-child")
            "mov rdi, {name_lo}",
            "mov rsi, {name_hi}",
            "mov rdx, 15",
            "syscall",
            "cmp rax, 0",
            "jl 2f",
            "mov r12, rax",
            "mov rax, 6", // SYS_PROCESS_START
            "mov rdi, r12",
            "syscall",
            "cmp rax, 0",
            "jne 2f",
            "mov rax, 7", // SYS_WAIT
            "mov rdi, r12",
            "syscall",
            "cmp rax, 0",
            "jne 2f",
            "cmp rdi, 0",  // kind == Exited?
            "jne 2f",
            "cmp rsi, 42", // code == 42?
            "jne 2f",

            // Success: report FRC_PRE_OK.
            "mov rax, 1",
            "mov rdi, 0",
            "mov rsi, 10",
            "mov rdx, {word0}",
            "mov r10, {ok_word1}",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "jmp 3f",
            // Failure: report FRC_PRE_FAIL.
            "2:",
            "mov rax, 1",
            "mov rdi, 0",
            "mov rsi, 12",
            "mov rdx, {word0}",
            "mov r10, {fail_word1}",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "3:",
            "mov rax, 3",
            "mov rdi, 0",
            "syscall",
            target_pid = const TARGET_PID,
            name_lo = const NAME_LO,
            name_hi = const NAME_HI,
            word0 = const WORD0,
            ok_word1 = const OK_WORD1,
            fail_word1 = const FAIL_WORD1,
            options(noreturn, nostack)
        );
    }
}
