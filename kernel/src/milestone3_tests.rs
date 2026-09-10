//! Ring-3 entry points for milestone 3's adversarial spawn/grant
//! boundary test (see `xtask`'s `test-spawn-boundary` command).
//!
//! Same constraints as `milestone2_tests`'s dummy processes: each stays
//! within a single remapped page (see `Process::new_dummy`'s doc
//! comment), every value is a `const` baked into the instruction stream
//! rather than computed at runtime, and there are no Rust-level function
//! calls anywhere in these bodies.
#![cfg(feature = "spawn-boundary-test")]
use core::arch::asm;

/// A real, running process that plays no other role than being *not*
/// [`boundary_test_process`]'s child — the target its first check aims
/// a `SYS_GRANT` at, to prove the syscall rejects a real target that
/// simply isn't a `Suspended` child of the caller, not just a
/// nonexistent one. Loops on `SYS_YIELD` forever (never `hlt`, which is
/// privileged and would fault immediately at CPL 3 — this process must
/// actually stay alive, not get killed and quietly leave the target
/// nonexistent instead of merely not-a-child); the test harness kills
/// QEMU on its own timeout once [`boundary_test_process`] has reported
/// its result.
pub unsafe extern "C" fn boundary_bystander_process() -> ! {
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

/// Runs four checks against `SYS_GRANT`/`SYS_SPAWN`/`SYS_PROCESS_START`'s
/// ownership and rights enforcement, entirely via raw `syscall`
/// instructions and register comparisons (no Rust-level branching is
/// available to a function confined to one page with no calls):
///
/// 1. `SYS_GRANT` against `boundary_bystander_process`'s `Pid` (a real,
///    running process, but not this process's child) — must fail with
///    `InvalidTarget`.
/// 2. `SYS_SPAWN("echo-child")` — a legitimate spawn, to get a real
///    `Suspended` child to aim the remaining checks at.
/// 3. `SYS_GRANT` against that child requesting `RECV` — a right this
///    process's own `CONSOLE_CAP` slot does not hold (`SEND` only), so
///    it must fail with `PermissionDenied`: rights can only be narrowed
///    on grant, never amplified.
/// 4. `SYS_GRANT` against the same child requesting only `SEND` (a
///    subset of what this process holds) — must succeed, followed by
///    `SYS_PROCESS_START` on it, which must also succeed: the mechanism
///    still works after the two rejected attempts above.
///
/// Reports `"BOUNDARY_OK"` (all four checks matched their expected
/// result) or `"BOUNDARY_FAIL"` on `CONSOLE_CAP`, then exits.
pub unsafe extern "C" fn boundary_test_process() -> ! {
    // Packed constants, computed by the (const-evaluated, not
    // runtime-called) `u64::from_le_bytes` intrinsic rather than
    // hand-typed hex, mirroring `milestone2_tests`'s `WORD` constants.
    const NAME_LO: u64 = u64::from_le_bytes(*b"echo-chi");
    const NAME_HI: u64 = u64::from_le_bytes(*b"ld\0\0\0\0\0\0");
    const WORD0: u64 = u64::from_le_bytes(*b"BOUNDARY");
    const OK_WORD1: u64 = u64::from_le_bytes(*b"_OK\0\0\0\0\0");
    const FAIL_WORD1: u64 = u64::from_le_bytes(*b"_FAIL\0\0\0");

    unsafe {
        asm!(
            // Check 1: SYS_GRANT(target=0 [the bystander, not our
            // child], src_cap=CONSOLE_CAP=0, dest_cap=0, rights=SEND).
            "mov rax, 5",
            "mov rdi, 0",
            "mov rsi, 0",
            "mov rdx, 0",
            "mov r10, 1",
            "syscall",
            "cmp rax, -6",
            "jne 2f",
            // Check 2: SYS_SPAWN("echo-child").
            "mov rax, 4",
            "mov rdi, {name_lo}",
            "mov rsi, {name_hi}",
            "mov rdx, 10",
            "syscall",
            "cmp rax, 0",
            "jl 2f",
            "mov r12, rax",
            // Check 3: SYS_GRANT(target=child, src_cap=CONSOLE_CAP,
            // dest_cap=0, rights=RECV) -- over-privileged.
            "mov rax, 5",
            "mov rdi, r12",
            "mov rsi, 0",
            "mov rdx, 0",
            "mov r10, 2",
            "syscall",
            "cmp rax, -3",
            "jne 2f",
            // Check 4a: SYS_GRANT(target=child, src_cap=CONSOLE_CAP,
            // dest_cap=0, rights=SEND) -- legitimate.
            "mov rax, 5",
            "mov rdi, r12",
            "mov rsi, 0",
            "mov rdx, 0",
            "mov r10, 1",
            "syscall",
            "cmp rax, 0",
            "jne 2f",
            // Check 4b: SYS_PROCESS_START(target=child).
            "mov rax, 6",
            "mov rdi, r12",
            "syscall",
            "cmp rax, 0",
            "jne 2f",
            // All four checks passed: report BOUNDARY_OK.
            "mov rax, 1",
            "mov rdi, 0",
            "mov rsi, 11",
            "mov rdx, {word0}",
            "mov r10, {ok_word1}",
            "mov r8, 0",
            "mov r9, 0",
            "syscall",
            "jmp 3f",
            "2:",
            "mov rax, 1",
            "mov rdi, 0",
            "mov rsi, 13",
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
