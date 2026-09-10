//! Ring-3 entry points for milestone 2's fault-isolation and
//! double-send integration tests (see `xtask`'s `test-fault-isolation`
//! and `test-double-send` commands).
//!
//! These run as `Process::new_dummy` processes — a kernel-compiled
//! function's own code page remapped user-executable — rather than
//! separate userland ELF binaries, so they make syscalls via raw
//! inline `syscall` instructions instead of linking `tarnos-rt`. Each
//! must stay small enough not to cross a page boundary (see
//! `Process::new_dummy`'s doc comment): every value here is either a
//! `const` (baked into the instruction stream as an immediate, not
//! computed at runtime) and every operation is a single inlined `asm!`
//! block with **no Rust-level function calls** — a call to anything not
//! on this same remapped page (even a small, normally-always-inlined
//! helper, in an unoptimized debug build) would itself fault trying to
//! fetch an instruction from an address this process's address space
//! never mapped.
#![cfg(any(feature = "fault-isolation-test", feature = "double-send-test"))]
use core::arch::asm;

/// Dereferences a guaranteed-unmapped address via a bare `mov`, with no
/// surrounding Rust code (not even `core::ptr::read_volatile`, which
/// risks compiling to an out-of-line call in an unoptimized build) that
/// could pull in a call to code outside this function's own page. Used
/// by `test-fault-isolation` as the process the kernel is expected to
/// kill without panicking itself.
#[cfg(feature = "fault-isolation-test")]
pub unsafe extern "C" fn faulting_process() -> ! {
    unsafe {
        asm!(
            "mov rax, 0x200000000000",
            "mov rax, [rax]",
            out("rax") _,
            options(nostack)
        );
    }
    // Unreachable (the read above always faults), but the function
    // still needs a `-> !`-compatible tail for the type to check.
    loop {
        unsafe {
            asm!("hlt", options(nomem, nostack));
        }
    }
}

/// Exits immediately via a raw `SYS_EXIT`. Used by `test-fault-isolation`
/// as the process that must still run to a clean completion after the
/// other dummy process faults.
#[cfg(feature = "fault-isolation-test")]
pub unsafe extern "C" fn survivor_process() -> ! {
    unsafe {
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_EXIT,
            in("rdi") 0u64,
            options(noreturn)
        );
    }
}

/// Sends a 4-byte tagged message on `CONSOLE_CAP` (slot 0) via a raw
/// `SYS_SEND`, then exits via `SYS_EXIT`. Used by `test-double-send` —
/// two of these, with distinct tags, sending with no receiver polled
/// yet, reproduce the historical double-send-panics bug.
#[cfg(feature = "double-send-test")]
pub unsafe extern "C" fn sender_process_a() -> ! {
    const WORD: u64 = u64::from_le_bytes(*b"MSGA\0\0\0\0");
    unsafe {
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_SEND,
            in("rdi") 0u64, // CONSOLE_CAP = CapIndex(0)
            in("rsi") 4u64, // tag: byte length of the 4-character message
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

#[cfg(feature = "double-send-test")]
pub unsafe extern "C" fn sender_process_b() -> ! {
    const WORD: u64 = u64::from_le_bytes(*b"MSGB\0\0\0\0");
    unsafe {
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_SEND,
            in("rdi") 0u64,
            in("rsi") 4u64,
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
