//! Ring-3 entry points for milestone 5's userland-heap integration tests
//! (see `xtask`'s `test-heap-growth` and `test-sbrk-boundary` commands).
//!
//! Same constraints as `milestone3_tests`/`milestone4_tests`'s dummy
//! processes: each stays within a single remapped page (see
//! `Process::new_dummy`'s doc comment), every multi-byte value is a
//! `const` baked into the instruction stream via the
//! `u64::from_le_bytes` intrinsic (const-evaluated, not a runtime call)
//! rather than hand-typed hex, and there are no Rust-level function
//! calls anywhere in these bodies.
#![cfg(any(feature = "heap-growth-test", feature = "sbrk-boundary-test"))]
use core::arch::asm;

/// Spawns `heap-child` (a real ELF process that builds a multi-page
/// `Vec<u64>` via `sys_sbrk`-backed `alloc`), releases it, and
/// `SYS_WAIT`s for it to finish. Asserts it exited `0` (its own internal
/// check that every value it wrote is still intact). Reports
/// `HEAP_OK`/`HEAP_FAIL` on `CONSOLE_CAP`, then exits. `heap-child` needs
/// no capabilities — it never does IPC — so no `SYS_GRANT` is needed.
#[cfg(feature = "heap-growth-test")]
pub unsafe extern "C" fn heap_growth_test_process() -> ! {
    const NAME_LO: u64 = u64::from_le_bytes(*b"heap-chi");
    const NAME_HI: u64 = u64::from_le_bytes(*b"ld\0\0\0\0\0\0");
    const OK_WORD0: u64 = u64::from_le_bytes(*b"HEAP_OK\0");
    const FAIL_WORD0: u64 = u64::from_le_bytes(*b"HEAP_FAI");
    const FAIL_WORD1: u64 = u64::from_le_bytes(*b"L\0\0\0\0\0\0\0");

    unsafe {
        asm!(
            // SYS_SPAWN("heap-child")
            "mov rax, 4",
            "mov rdi, {name_lo}",
            "mov rsi, {name_hi}",
            "mov rdx, 10",
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
            "cmp rdi, 0", // kind == Exited?
            "jne 2f",
            "cmp rsi, 0", // code == 0 (heap-child's own success check)?
            "jne 2f",
            // Success: report HEAP_OK.
            "mov rax, 1",
            "mov rdi, 0",
            "mov rsi, 7",
            "mov rdx, {ok_word0}",
            "mov r10, 0",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "jmp 3f",
            // Failure: report HEAP_FAIL.
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

/// Runs four adversarial checks against `SYS_SBRK` directly (no child
/// process needed — it operates on the caller's own heap):
///
/// 1. An increment comfortably over the fixed 64 MiB heap ceiling is
///    rejected with `InvalidArgument`, before any frame is touched.
/// 2. A small, valid grow succeeds; its returned (previous) break is
///    saved for check 4's cross-check.
/// 3. A negative increment is rejected with `InvalidArgument` — grow-only
///    this milestone, no real shrink.
/// 4. A zero increment is a side-effect-free query: it must return
///    exactly check 2's returned break plus check 2's own grow amount —
///    proving checks 1 and 3 truly mutated nothing in between.
///
/// Reports `SBRK_OK`/`SBRK_FAIL` on `CONSOLE_CAP`, then exits.
#[cfg(feature = "sbrk-boundary-test")]
pub unsafe extern "C" fn sbrk_boundary_test_process() -> ! {
    const OK_WORD0: u64 = u64::from_le_bytes(*b"SBRK_OK\0");
    const FAIL_WORD0: u64 = u64::from_le_bytes(*b"SBRK_FAI");
    const FAIL_WORD1: u64 = u64::from_le_bytes(*b"L\0\0\0\0\0\0\0");
    const GROW_BYTES: u64 = 3 * 4096;
    // Double the 64 MiB heap ceiling -- must be rejected outright.
    const ABSURD_INCREMENT: u64 = 128 * 1024 * 1024;

    unsafe {
        asm!(
            // Check 1: absurd increment -> InvalidArgument, zero frames touched.
            "mov rax, 9",
            "mov rdi, {absurd}",
            "syscall",
            "cmp rax, -8",
            "jne 2f",

            // Check 2: a small valid grow succeeds; keep its returned
            // (previous) break in r12.
            "mov rax, 9",
            "mov rdi, {grow}",
            "syscall",
            "cmp rax, 0",
            "jl 2f",
            "mov r12, rax",

            // Check 3: negative increment -> InvalidArgument (grow-only).
            "mov rax, 9",
            "mov rdi, -4096",
            "syscall",
            "cmp rax, -8",
            "jne 2f",

            // Check 4: zero increment is a side-effect-free query --
            // must equal check 2's break plus check 2's own grow amount.
            "mov rax, 9",
            "mov rdi, 0",
            "syscall",
            "cmp rax, 0",
            "jl 2f",
            "mov r13, r12",
            "add r13, {grow}",
            "cmp rax, r13",
            "jne 2f",

            // All checks passed: report SBRK_OK.
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
            absurd = const ABSURD_INCREMENT,
            grow = const GROW_BYTES,
            ok_word0 = const OK_WORD0,
            fail_word0 = const FAIL_WORD0,
            fail_word1 = const FAIL_WORD1,
            options(noreturn, nostack)
        );
    }
}
