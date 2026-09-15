//! Ring-3 entry points for milestone 9's "kitchen sink" integration test
//! (see `xtask`'s `test-kitchen-sink` command): several small
//! orchestrator processes, each driving a *different*, already-proven-
//! solid workload concurrently rather than in isolation, reusing the
//! real userland ELF programs wherever possible instead of new raw-asm
//! dummy processes.
//!
//! Same constraints as every other milestone's dummy processes: each
//! stays within a single remapped page, every multi-byte value is a
//! `const` baked into the instruction stream, and there are no
//! Rust-level function calls anywhere in these bodies.
#![cfg(feature = "kitchen-sink-test")]
use core::arch::asm;

/// The cap index (in each orchestrator's own, otherwise-empty table)
/// boot code seeds a `SEND | RECV` capability on a fresh, dedicated
/// endpoint at — mirrors `tarnos_abi::CHILD_LINK_CAP`'s own role for
/// `init`, just per-orchestrator rather than kernel-wide.
const CHILD_LINK_CAP: u64 = 1;

/// Spawns `echo-child`, grants it a link capability, releases it, and
/// completes a real IPC round trip with it -- the exact same dance
/// `userland/init` already does, just via raw `syscall` instructions
/// (a `Process::new_dummy` body can't link `tarnos-rt`) and reporting
/// `KS_IPC__OK`/`KS_IPC__FAIL` on `CONSOLE_CAP` instead of just
/// `init`'s own greeting text.
#[cfg(feature = "kitchen-sink-test")]
pub unsafe extern "C" fn ks_ipc_orchestrator() -> ! {
    const NAME_LO: u64 = u64::from_le_bytes(*b"echo-chi");
    const NAME_HI: u64 = u64::from_le_bytes(*b"ld\0\0\0\0\0\0");
    const PING: u64 = u64::from_le_bytes(*b"ping\0\0\0\0");
    const PONG: u64 = u64::from_le_bytes(*b"pong\0\0\0\0");
    const WORD0: u64 = u64::from_le_bytes(*b"KS_IPC__");
    const OK_WORD1: u64 = u64::from_le_bytes(*b"OK\0\0\0\0\0\0");
    const FAIL_WORD1: u64 = u64::from_le_bytes(*b"FAIL\0\0\0\0");

    unsafe {
        asm!(
            "mov rax, 4", // SYS_SPAWN("echo-child")
            "mov rdi, {name_lo}",
            "mov rsi, {name_hi}",
            "mov rdx, 10",
            "syscall",
            "cmp rax, 0",
            "jl 2f",
            "mov r12, rax", // r12 = child_pid, preserved across the calls below

            "mov rax, 5", // SYS_GRANT(child_pid, src_cap=1, dest_cap=0, SEND|RECV)
            "mov rdi, r12",
            "mov rsi, {child_link_cap}",
            "mov rdx, 0",
            "mov r10, 3", // Rights::SEND | Rights::RECV
            "syscall",
            "cmp rax, 0",
            "jne 2f",

            "mov rax, 6", // SYS_PROCESS_START
            "mov rdi, r12",
            "syscall",
            "cmp rax, 0",
            "jne 2f",

            "mov rax, 1", // SYS_SEND(cap=1, "ping")
            "mov rdi, {child_link_cap}",
            "mov rsi, 4",
            "mov rdx, {ping}",
            "mov r10, 0",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "cmp rax, 0",
            "jne 2f",

            "mov rax, 2", // SYS_RECV(cap=1) -- expect "pong"
            "mov rdi, {child_link_cap}",
            "syscall",
            "cmp rax, 0",
            "jne 2f",
            "cmp rdi, 4",
            "jne 2f",
            "cmp rsi, {pong}",
            "jne 2f",

            "mov rax, 1", // report KS_IPC__OK
            "mov rdi, 0",
            "mov rsi, 10",
            "mov rdx, {word0}",
            "mov r10, {ok_word1}",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "jmp 3f",
            "2:",
            "mov rax, 1", // report KS_IPC__FAIL
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
            name_lo = const NAME_LO,
            name_hi = const NAME_HI,
            child_link_cap = const CHILD_LINK_CAP,
            ping = const PING,
            pong = const PONG,
            word0 = const WORD0,
            ok_word1 = const OK_WORD1,
            fail_word1 = const FAIL_WORD1,
            options(noreturn, nostack)
        );
    }
}

/// Spawns `heap-child`, starts it (it needs no capabilities), and waits
/// for it to prove its `sys_sbrk`-backed `Vec<u64>` survived several
/// heap growths intact -- exactly `xtask test-heap-growth`'s own check,
/// just running concurrently with everything else here. Reports
/// `KS_HEAP_OK`/`KS_HEAP_FAIL` on `CONSOLE_CAP`.
#[cfg(feature = "kitchen-sink-test")]
pub unsafe extern "C" fn ks_heap_orchestrator() -> ! {
    const NAME_LO: u64 = u64::from_le_bytes(*b"heap-chi");
    const NAME_HI: u64 = u64::from_le_bytes(*b"ld\0\0\0\0\0\0");
    const WORD0: u64 = u64::from_le_bytes(*b"KS_HEAP_");
    const OK_WORD1: u64 = u64::from_le_bytes(*b"OK\0\0\0\0\0\0");
    const FAIL_WORD1: u64 = u64::from_le_bytes(*b"FAIL\0\0\0\0");

    unsafe {
        asm!(
            "mov rax, 4", // SYS_SPAWN("heap-child")
            "mov rdi, {name_lo}",
            "mov rsi, {name_hi}",
            "mov rdx, 10",
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
            "cmp rsi, 0",  // code == 0 (heap-child's own success code)?
            "jne 2f",

            "mov rax, 1", // report KS_HEAP_OK
            "mov rdi, 0",
            "mov rsi, 10",
            "mov rdx, {word0}",
            "mov r10, {ok_word1}",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "jmp 3f",
            "2:",
            "mov rax, 1", // report KS_HEAP_FAIL
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
            name_lo = const NAME_LO,
            name_hi = const NAME_HI,
            word0 = const WORD0,
            ok_word1 = const OK_WORD1,
            fail_word1 = const FAIL_WORD1,
            options(noreturn, nostack)
        );
    }
}

/// Repeats a bounded spawn+start+wait cycle against `exit-code-child`
/// three times in a row, checking its exit code each time -- a bounded
/// lifecycle loop running concurrently with everything else, rather than
/// `xtask test-wait-exit-code`'s own one-shot check. Reports
/// `KS_LIFE_OK`/`KS_LIFE_FAIL` on `CONSOLE_CAP`.
#[cfg(feature = "kitchen-sink-test")]
pub unsafe extern "C" fn ks_lifecycle_orchestrator() -> ! {
    const NAME_LO: u64 = u64::from_le_bytes(*b"exit-cod");
    const NAME_HI: u64 = u64::from_le_bytes(*b"e-child\0");
    const WORD0: u64 = u64::from_le_bytes(*b"KS_LIFE_");
    const OK_WORD1: u64 = u64::from_le_bytes(*b"OK\0\0\0\0\0\0");
    const FAIL_WORD1: u64 = u64::from_le_bytes(*b"FAIL\0\0\0\0");
    const ITERATIONS: u64 = 3;

    unsafe {
        asm!(
            "mov r13, 0", // iteration counter
            "5:",
            "cmp r13, {iterations}",
            "jge 4f", // all iterations succeeded

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

            "inc r13",
            "jmp 5b",

            "4:",
            "mov rax, 1", // report KS_LIFE_OK
            "mov rdi, 0",
            "mov rsi, 10",
            "mov rdx, {word0}",
            "mov r10, {ok_word1}",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "jmp 3f",
            "2:",
            "mov rax, 1", // report KS_LIFE_FAIL
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
            iterations = const ITERATIONS,
            name_lo = const NAME_LO,
            name_hi = const NAME_HI,
            word0 = const WORD0,
            ok_word1 = const OK_WORD1,
            fail_word1 = const FAIL_WORD1,
            options(noreturn, nostack)
        );
    }
}

/// Free-spins on `SYS_YIELD` forever, never exiting on its own --
/// identical in shape to `milestone7_tests::kill_cc_target_process`,
/// duplicated here (rather than shared across features) so this file
/// stays self-contained. `ks_kill_orchestrator` kills it outright while
/// it's genuinely `Running`, exercising the same cross-core eviction
/// protocol concurrently with every other workload in this scenario
/// rather than in isolation.
#[cfg(feature = "kitchen-sink-test")]
pub unsafe extern "C" fn ks_kill_target_process() -> ! {
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

/// Yields a few times (letting `ks_kill_target_process` actually start
/// running elsewhere), then `SYS_KILL`s it. Reports
/// `KS_KILL_OK`/`KS_KILL_FAIL` on `CONSOLE_CAP`.
///
/// Boot code makes this process `ks_kill_target_process`'s parent
/// directly and allocates the target's `Pid` first (before spawning
/// anything else in this scenario), so `TARGET_PID` below (table index
/// 0, generation 1) is the same fixed-value trick
/// `milestone7_tests::kill_cc_killer_process` already relies on.
#[cfg(feature = "kitchen-sink-test")]
pub unsafe extern "C" fn ks_kill_orchestrator() -> ! {
    const TARGET_PID: u64 = 0 | (1u64 << 32);
    const WORD0: u64 = u64::from_le_bytes(*b"KS_KILL_");
    const OK_WORD1: u64 = u64::from_le_bytes(*b"OK\0\0\0\0\0\0");
    const FAIL_WORD1: u64 = u64::from_le_bytes(*b"FAIL\0\0\0\0");

    unsafe {
        asm!(
            "mov rax, 0", "syscall",
            "mov rax, 0", "syscall",
            "mov rax, 0", "syscall",
            "mov rax, 0", "syscall",
            "mov rax, 0", "syscall",

            "mov rax, 8", // SYS_KILL(target)
            "mov rdi, {target_pid}",
            "syscall",
            "cmp rax, 0",
            "jne 2f",

            "mov rax, 1", // report KS_KILL_OK
            "mov rdi, 0",
            "mov rsi, 10",
            "mov rdx, {word0}",
            "mov r10, {ok_word1}",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "jmp 3f",
            "2:",
            "mov rax, 1", // report KS_KILL_FAIL
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
            word0 = const WORD0,
            ok_word1 = const OK_WORD1,
            fail_word1 = const FAIL_WORD1,
            options(noreturn, nostack)
        );
    }
}

/// Free-spins on `SYS_YIELD` a fixed number of times, then exits --
/// identical in shape to `milestone7_tests::concurrency_process_a`/
/// `_b`. Boot code spawns a couple of these purely as background
/// scheduling pressure, keeping cores genuinely busy with cooperative
/// preemption alongside the rest of this scenario. Reports nothing; its
/// only role is load.
///
/// Deliberately a fraction of `concurrency_process_a`/`_b`'s own
/// iteration count (100,000 each, at `-smp 4` in `test-smp-sched-
/// concurrency`): those two processes are the *entire* workload for
/// that scenario, where kitchen-sink's real value is several *different*
/// subsystems (IPC, heap growth, spawn/wait lifecycle, a cross-core
/// kill) proving they hold up running concurrently with each other --
/// background `SYS_YIELD` pressure only needs to be enough to keep every
/// core genuinely contended while that happens, not to dominate the
/// scenario's own runtime. An earlier version reused the same 50,000-
/// iteration, four-process scale as a from-scratch guess and found it
/// pushed this test's total scheduling-tick volume far past every
/// comparable scenario's (`test-smp-sched-concurrency` included), most
/// of it spent on pressure load contributing nothing to what the test
/// actually verifies -- see `docs/adr/0012`.
#[cfg(feature = "kitchen-sink-test")]
pub unsafe extern "C" fn ks_pressure_process() -> ! {
    unsafe {
        asm!(
            "mov r13, 0",
            "2:",
            "cmp r13, 5000",
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
