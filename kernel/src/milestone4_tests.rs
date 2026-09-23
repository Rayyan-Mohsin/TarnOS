//! Ring-3 entry points for milestone 4's process-lifecycle integration
//! tests (see `xtask`'s `test-process-lifecycle`, `test-wait-exit-code`,
//! and `test-kill-boundary` commands).
//!
//! Same constraints as `milestone2_tests`/`milestone3_tests`'s dummy
//! processes: each stays within a single remapped page (see
//! `Process::new_dummy`'s doc comment), every multi-byte value is a
//! `const` baked into the instruction stream via the
//! `u64::from_le_bytes` intrinsic (const-evaluated, not a runtime call)
//! rather than hand-typed hex, and there are no Rust-level function
//! calls anywhere in these bodies.
#![cfg(any(
    feature = "process-lifecycle-test",
    feature = "wait-exit-code-test",
    feature = "kill-boundary-test"
))]
use core::arch::asm;

/// A real, running process whose only role is being *not* the test
/// process's child — [`kill_boundary_test_process`]'s first check aims
/// a `SYS_KILL` at it to prove the syscall rejects a real target that
/// simply isn't the caller's child. Loops on `SYS_YIELD` forever (never
/// `hlt`, which is privileged and would fault immediately at CPL 3).
#[cfg(feature = "kill-boundary-test")]
pub unsafe extern "C" fn kill_test_bystander_process() -> ! {
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

/// Repeatedly spawns a `Suspended` `echo-child` and immediately kills
/// it, `LIFECYCLE_ITERATIONS` times in a row. At most two process-table
/// slots (this process's own, plus whichever child is momentarily
/// alive) are ever occupied at once — if terminating a process didn't
/// actually free its slot (the memory-leak/table-leak this milestone
/// fixes), `SYS_SPAWN` would report `SpawnFailed`/`ResourceExhausted`
/// well before the loop completes, since `MAX_PROCESSES` is far smaller
/// than the iteration count. Reports `LIFECYCLE_OK`/`LIFECYCLE_FAIL` on
/// `CONSOLE_CAP`, then exits.
#[cfg(feature = "process-lifecycle-test")]
pub unsafe extern "C" fn lifecycle_test_process() -> ! {
    const NAME_LO: u64 = u64::from_le_bytes(*b"echo-chi");
    const NAME_HI: u64 = u64::from_le_bytes(*b"ld\0\0\0\0\0\0");
    const WORD0: u64 = u64::from_le_bytes(*b"LIFECYCL");
    const OK_WORD1: u64 = u64::from_le_bytes(*b"E_OK\0\0\0\0");
    const FAIL_WORD1: u64 = u64::from_le_bytes(*b"E_FAIL\0\0");

    unsafe {
        asm!(
            "mov r13, 0",
            "2:", // loop_start
            "cmp r13, 48", // 3 * MAX_PROCESSES
            "jge 3f",      // -> success
            // SYS_SPAWN("echo-child")
            "mov rax, 4",
            "mov rdi, {name_lo}",
            "mov rsi, {name_hi}",
            "mov rdx, 10",
            "syscall",
            "cmp rax, 0",
            "jl 4f", // -> fail
            "mov r12, rax",
            // SYS_KILL(child) -- while still Suspended, never started.
            // This makes it a reapable Zombie (it has a parent: us) --
            // the same as real Unix semantics for kill()+wait() -- so
            // SYS_WAIT right after is what actually frees its slot back
            // to Empty for reuse; skipping it would exhaust the process
            // table on zombies well before 48 iterations.
            "mov rax, 8",
            "mov rdi, r12",
            "syscall",
            "cmp rax, 0",
            "jne 4f",
            "mov rax, 7", // SYS_WAIT -- reap the zombie just created
            "mov rdi, r12",
            "syscall",
            "cmp rax, 0",
            "jne 4f",
            "inc r13",
            "jmp 2b",
            // Success: report LIFECYCLE_OK.
            "3:",
            "mov rax, 1",
            "mov rdi, 0",
            "mov rsi, 12",
            "mov rdx, {word0}",
            "mov r10, {ok_word1}",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "jmp 5f",
            // Failure: report LIFECYCLE_FAIL.
            "4:",
            "mov rax, 1",
            "mov rdi, 0",
            "mov rsi, 14",
            "mov rdx, {word0}",
            "mov r10, {fail_word1}",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "5:",
            "mov rax, 3",
            "mov rdi, 0",
            "syscall",
            name_lo = const NAME_LO,
            name_hi = const NAME_HI,
            word0 = const WORD0,
            ok_word1 = const OK_WORD1,
            fail_word1 = const FAIL_WORD1,
            options(noreturn, nostack)
        );
    }
}

/// Spawns `exit-code-child`, releases it, and immediately `SYS_WAIT`s on
/// it — before any `SYS_YIELD`, so the wait deterministically finds the
/// child hasn't run yet and must genuinely block (mirroring
/// `milestone2_tests`' `test-blocking-ipc` trick of forcing the
/// suspend-and-resume path rather than merely a non-blocking read of an
/// already-`Zombie` slot). Asserts the returned status is
/// `Exited(42)`. Reports `WAIT_OK`/`WAIT_FAIL` on `CONSOLE_CAP`, then
/// exits.
#[cfg(feature = "wait-exit-code-test")]
pub unsafe extern "C" fn wait_test_process() -> ! {
    const NAME_LO: u64 = u64::from_le_bytes(*b"exit-cod");
    const NAME_HI: u64 = u64::from_le_bytes(*b"e-child\0");
    const OK_WORD0: u64 = u64::from_le_bytes(*b"WAIT_OK\0");
    const FAIL_WORD0: u64 = u64::from_le_bytes(*b"WAIT_FAI");
    const FAIL_WORD1: u64 = u64::from_le_bytes(*b"L\0\0\0\0\0\0\0");

    unsafe {
        asm!(
            // SYS_SPAWN("exit-code-child")
            "mov rax, 4",
            "mov rdi, {name_lo}",
            "mov rsi, {name_hi}",
            "mov rdx, 15",
            "syscall",
            "cmp rax, 0",
            "jl 2f", // -> fail
            "mov r12, rax",
            // SYS_PROCESS_START(child)
            "mov rax, 6",
            "mov rdi, r12",
            "syscall",
            "cmp rax, 0",
            "jne 2f",
            // SYS_WAIT(child)
            "mov rax, 7",
            "mov rdi, r12",
            "syscall",
            "cmp rax, 0",
            "jne 2f",
            "cmp rdi, 0",  // kind == Exited?
            "jne 2f",
            "cmp rsi, 42", // code == 42?
            "jne 2f",
            // Success: report WAIT_OK.
            "mov rax, 1",
            "mov rdi, 0",
            "mov rsi, 7",
            "mov rdx, {ok_word0}",
            "mov r10, 0",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "jmp 3f",
            // Failure: report WAIT_FAIL.
            "2:",
            "mov rax, 1",
            "mov rdi, 0",
            "mov rsi, 9",
            "mov rdx, {fail_word0}",
            "mov r10, {fail_word1}",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "3:",
            "mov rax, 3",
            "mov rdi, 0",
            "syscall",
            name_lo = const NAME_LO,
            name_hi = const NAME_HI,
            ok_word0 = const OK_WORD0,
            fail_word0 = const FAIL_WORD0,
            fail_word1 = const FAIL_WORD1,
            options(noreturn, nostack)
        );
    }
}

/// Runs four checks against `SYS_KILL`, entirely via raw `syscall`
/// instructions and register comparisons:
///
/// 1. `SYS_KILL` against [`kill_test_bystander_process`]'s `Pid` (a
///    real, running process, but not this process's child) — must fail
///    with `InvalidTarget`.
/// 2. `SYS_SPAWN("echo-child")`, then `SYS_KILL` it immediately, while
///    still `Suspended` (never started) — must succeed.
/// 3. `SYS_SPAWN("echo-child")` again, `SYS_GRANT` it `SEND | RECV` on
///    this process's own second capability, `SYS_PROCESS_START` it, and
///    `SYS_YIELD` a few times so it actually runs far enough to block
///    inside its own `sys_recv` (transitioning it to genuinely
///    `Blocked`, queued inside the granted `Endpoint`) — then `SYS_KILL`
///    it. Must succeed against a real `Blocked` target, not just a
///    `Suspended` one.
///
/// Reports `KILL_OK`/`KILL_FAIL` on `CONSOLE_CAP`, then exits. The
/// strongest implicit check is simply that the whole run reaches this
/// point at all with no kernel panic: a missing ready-queue removal or
/// generation check would eventually surface as `switch_to`'s own
/// `.expect(...)` panicking on a stale `Pid`.
#[cfg(feature = "kill-boundary-test")]
pub unsafe extern "C" fn kill_boundary_test_process() -> ! {
    const NAME_LO: u64 = u64::from_le_bytes(*b"echo-chi");
    const NAME_HI: u64 = u64::from_le_bytes(*b"ld\0\0\0\0\0\0");
    const OK_WORD0: u64 = u64::from_le_bytes(*b"KILL_OK\0");
    const FAIL_WORD0: u64 = u64::from_le_bytes(*b"KILL_FAI");
    const FAIL_WORD1: u64 = u64::from_le_bytes(*b"L\0\0\0\0\0\0\0");
    // See the identical constant in `milestone3_tests::boundary_test_process`:
    // the bystander is always this feature's very first-ever `allocate_pid()`
    // call (table index 0), which `allocate_pid` hands out at generation 1,
    // not 0.
    const BYSTANDER_PID: u64 = 0 | (1u64 << 32);

    unsafe {
        asm!(
            // Check 1: SYS_KILL(bystander, not our child).
            "mov rax, 8",
            "mov rdi, {bystander_pid}",
            "syscall",
            "cmp rax, -6",
            "jne 2f", // -> fail

            // Check 2: SYS_SPAWN + immediate SYS_KILL of a Suspended child.
            "mov rax, 4",
            "mov rdi, {name_lo}",
            "mov rsi, {name_hi}",
            "mov rdx, 10",
            "syscall",
            "cmp rax, 0",
            "jl 2f",
            "mov r12, rax",
            "mov rax, 8",
            "mov rdi, r12",
            "syscall",
            "cmp rax, 0",
            "jne 2f",

            // Check 3: SYS_SPAWN + SYS_GRANT + SYS_PROCESS_START, yield
            // until it blocks, then SYS_KILL a genuinely Blocked child.
            "mov rax, 4",
            "mov rdi, {name_lo}",
            "mov rsi, {name_hi}",
            "mov rdx, 10",
            "syscall",
            "cmp rax, 0",
            "jl 2f",
            "mov r12, rax",
            "mov rax, 5",
            "mov rdi, r12",
            "mov rsi, 1", // this process's own second cap slot
            "mov rdx, 0", // into the child's slot 0
            "mov r10, 3", // SEND | RECV
            "syscall",
            "cmp rax, 0",
            "jne 2f",
            "mov rax, 6",
            "mov rdi, r12",
            "syscall",
            "cmp rax, 0",
            "jne 2f",
            "mov rax, 0", // SYS_YIELD, x3 -- let the child run and block
            "syscall",
            "mov rax, 0",
            "syscall",
            "mov rax, 0",
            "syscall",
            "mov rax, 8",
            "mov rdi, r12",
            "syscall",
            "cmp rax, 0",
            "jne 2f",

            // All checks passed: report KILL_OK.
            "mov rax, 1",
            "mov rdi, 0",
            "mov rsi, 7",
            "mov rdx, {ok_word0}",
            "mov r10, 0",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "jmp 3f",
            "2:",
            "mov rax, 1",
            "mov rdi, 0",
            "mov rsi, 9",
            "mov rdx, {fail_word0}",
            "mov r10, {fail_word1}",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "3:",
            "mov rax, 3",
            "mov rdi, 0",
            "syscall",
            name_lo = const NAME_LO,
            name_hi = const NAME_HI,
            ok_word0 = const OK_WORD0,
            fail_word0 = const FAIL_WORD0,
            fail_word1 = const FAIL_WORD1,
            bystander_pid = const BYSTANDER_PID,
            options(noreturn, nostack)
        );
    }
}
