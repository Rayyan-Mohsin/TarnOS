//! Ring-3 entry points for milestone 7's cross-core scheduling
//! integration tests (see `xtask`'s `test-smp-sched-concurrency` and
//! `test-smp-kill-cross-core` commands).
//!
//! Same constraints as `milestone2_tests`/`milestone3_tests`/
//! `milestone4_tests`'s dummy processes: each stays within a single
//! remapped page (see `Process::new_dummy`'s doc comment), every
//! multi-byte value is a `const` baked into the instruction stream, and
//! there are no Rust-level function calls anywhere in these bodies.
#![cfg(any(
    feature = "smp-sched-concurrency-test",
    feature = "kill-cross-core-test"
))]
use core::arch::asm;

/// Free-spins on `SYS_YIELD` a large, fixed number of times, then exits.
/// Two of these (`concurrency_process_a`/`_b`) are spawned back-to-back
/// by boot code — since each `spawn()` call individually notifies every
/// currently-idle core (see `task::scheduler::notify_idle_cores`), and
/// whichever core grabs the first one is no longer idle by the time the
/// second spawn's notification goes out, they reliably land on two
/// *different* cores rather than racing for the same one. Boot code
/// polls `percpu::PerCpuSlot.current` on every core while these run,
/// looking for direct evidence both are resident on different cores at
/// the same instant — see `xtask test-smp-sched-concurrency`.
///
/// The exact iteration count only needs to comfortably outlast that
/// polling window (a couple of hundred milliseconds); it's deliberately
/// not huge, so the rest of boot (and this scenario's own timeout)
/// isn't held up waiting for it.
#[cfg(feature = "smp-sched-concurrency-test")]
pub unsafe extern "C" fn concurrency_process_a() -> ! {
    unsafe {
        asm!(
            "mov r13, 0",
            "2:",
            "cmp r13, 100000",
            "jge 3f",
            "mov rax, 0", // SYS_YIELD
            "syscall",
            "inc r13",
            "jmp 2b",
            "3:",
            "mov rax, 3", // SYS_EXIT
            "mov rdi, 0",
            "syscall",
            options(noreturn, nostack)
        );
    }
}

#[cfg(feature = "smp-sched-concurrency-test")]
pub unsafe extern "C" fn concurrency_process_b() -> ! {
    unsafe {
        asm!(
            "mov r13, 0",
            "2:",
            "cmp r13, 100000",
            "jge 3f",
            "mov rax, 0", // SYS_YIELD
            "syscall",
            "inc r13",
            "jmp 2b",
            "3:",
            "mov rax, 3", // SYS_EXIT
            "mov rdi, 0",
            "syscall",
            options(noreturn, nostack)
        );
    }
}

/// Free-spins on `SYS_YIELD` forever, never exiting on its own — the
/// target [`kill_cc_killer_process`] kills outright while it's
/// genuinely `Running`, most likely on a different core (having been
/// picked up off the shared ready queue by whichever core happened to
/// be idle when [`kill_cc_killer_process`] released it). This is what
/// actually exercises `task::scheduler::terminate_process`'s cross-core
/// eviction protocol (targeted IPI + bounded wait for the owning core to
/// confirm) rather than just the same-core immediate-finalize path
/// every earlier milestone's `SYS_KILL` tests already covered.
#[cfg(feature = "kill-cross-core-test")]
pub unsafe extern "C" fn kill_cc_target_process() -> ! {
    loop {
        unsafe {
            asm!(
                "syscall",
                in("rax") tarnos_abi::SYS_YIELD,
                out("rcx") _,
                out("r11") _,
                options(nostack, preserves_flags)
            );
        }
    }
}

/// Yields a few times (letting [`kill_cc_target_process`] actually start
/// running elsewhere), then `SYS_KILL`s it -- the direct adversarial
/// test for cross-core eviction. Immediately afterward, spawns, starts,
/// and waits on a completely ordinary child (`exit-code-child`) to prove
/// the machine is still fully healthy right after the eviction: a
/// missing or broken fix for the idle-core stale-CR3 hazard (see
/// `docs/adr/0010-cross-core-scheduling.md`) would surface as a spurious
/// kernel-mode page fault exactly here, quite possibly on the very core
/// that was just evicted. Reports `KILL_CC_OK`/`KILL_CC_FAIL` on
/// `CONSOLE_CAP`, then exits.
///
/// Boot code makes this process [`kill_cc_target_process`]'s parent
/// directly (both are `Process::new_dummy`s, not `SYS_SPAWN`ed, so
/// there's no other way to establish that relationship) and allocates
/// the target's `Pid` first, so `TARGET_PID` below (table index 0,
/// generation 1) is exactly the same fixed-value trick
/// `milestone4_tests::kill_boundary_test_process` already relies on for
/// its own hardcoded bystander `Pid`.
#[cfg(feature = "kill-cross-core-test")]
pub unsafe extern "C" fn kill_cc_killer_process() -> ! {
    const TARGET_PID: u64 = 0 | (1u64 << 32);
    const NAME_LO: u64 = u64::from_le_bytes(*b"exit-cod");
    const NAME_HI: u64 = u64::from_le_bytes(*b"e-child\0");
    const WORD0: u64 = u64::from_le_bytes(*b"KILL_CC_");
    const OK_WORD1: u64 = u64::from_le_bytes(*b"OK\0\0\0\0\0\0");
    const FAIL_WORD1: u64 = u64::from_le_bytes(*b"FAIL\0\0\0\0");

    unsafe {
        asm!(
            // Let the target actually get picked up and start running
            // (very likely on a different, previously-idle core) before
            // killing it.
            "mov rax, 0", "syscall",
            "mov rax, 0", "syscall",
            "mov rax, 0", "syscall",
            "mov rax, 0", "syscall",
            "mov rax, 0", "syscall",
            // SYS_KILL(target) -- must succeed against a genuinely
            // Running, likely cross-core target.
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

            // Success: report KILL_CC_OK.
            "mov rax, 1",
            "mov rdi, 0",
            "mov rsi, 10",
            "mov rdx, {word0}",
            "mov r10, {ok_word1}",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "jmp 3f",
            // Failure: report KILL_CC_FAIL.
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
