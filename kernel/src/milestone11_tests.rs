//! Ring-3 entry point for Milestone 11 Phase 4's `SYS_BLOCK_READ`
//! syscall test (see `xtask`'s `test-block-syscall` command).
//!
//! Same constraints as `milestone7_tests`/`milestone8_tests`'s dummy
//! processes: stays within the two pages `Process::new_dummy` copies,
//! every multi-byte value is a `const` baked into the instruction
//! stream, and there are no Rust-level function calls anywhere in this
//! body (a real call would jump outside the copied page(s), into
//! whatever kernel code happened to compile next to it) -- the read-back
//! content check below is a raw indexing loop for exactly that reason,
//! not an iterator adapter that could monomorphize into a separate,
//! not-copied function.
#![cfg(feature = "block-syscall-test")]
use core::arch::asm;

/// Issues a real `SYS_BLOCK_READ` for one sector at the fixed LBA
/// `xtask::create_test_disk_image` seeds with a known `0..=255`-
/// repeating pattern (the same convention `main.rs`'s Phase 3
/// kernel-internal smoke test and `xtask` itself already use), into a
/// stack-local buffer, then reports whether the content matches exactly
/// via `CONSOLE_CAP` (index 0) before exiting. `BLOCK_CAP` (index 2) is
/// seeded directly into this process's own table by `main.rs`'s own
/// `block-syscall-test` boot block -- this process never sees
/// `init`/`SYS_GRANT` at all.
pub unsafe extern "C" fn block_read_syscall_process() -> ! {
    const KNOWN_TEST_LBA: u64 = 2;
    // `MaybeUninit`, not `[0u8; 512]`: a zero-initialized array literal
    // this large lowers (in an unoptimized debug build, which is what
    // every `xtask` scenario actually builds) to a real `memset` call --
    // a jump outside the two pages `Process::new_dummy` copies, into
    // whatever kernel code happens to compile next to this function,
    // which is exactly the class of bug this module's own doc comment
    // warns against. Nothing here needs the buffer's initial content:
    // `SYS_BLOCK_READ` either overwrites all 512 bytes before this code
    // ever reads them (`result == 0`), or the read below is skipped
    // entirely (`content_matches` starts `false` on any other `result`).
    let mut buf: core::mem::MaybeUninit<[u8; 512]> = core::mem::MaybeUninit::uninit();
    let buf_ptr = buf.as_mut_ptr() as u64;
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_BLOCK_READ,
            in("rdi") 2u64, // BLOCK_CAP
            in("rsi") KNOWN_TEST_LBA,
            in("rdx") buf_ptr,
            in("r10") 1u64, // one sector
            in("r8") 0u64,
            in("r9") 0u64,
            lateout("rax") result,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }

    let mut content_matches = result == 0;
    if content_matches {
        // SAFETY: `result == 0` means `SYS_BLOCK_READ` reported success,
        // which per its own contract means it wrote all 512 bytes of
        // `buf` before returning.
        let buf = unsafe { buf.assume_init_ref() };
        let mut i = 0usize;
        while i < 512 {
            if buf[i] != (i % 256) as u8 {
                content_matches = false;
                break;
            }
            i += 1;
        }
    }

    // Both possible outcomes are top-level `const`s, never a byte-string
    // literal evaluated inside the runtime `if` below: a `const` is
    // always fully evaluated at compile time and inlined as an
    // immediate, guaranteed by the language regardless of optimization
    // level -- an un-`const`-bound literal is not guaranteed the same
    // in an unoptimized build, and could instead become a reference to
    // static data outside this function's own copied pages (the same
    // class of hazard `buf` above avoids for the same reason).
    const WORD0: u64 = u64::from_le_bytes(*b"BLK_SYSC");
    const WORD1_OK: u64 = u64::from_le_bytes(*b"ALL_OK\0\0"); // "BLK_SYSCALL_OK"
    const WORD1_FAIL: u64 = u64::from_le_bytes(*b"ALL_FAIL"); // "BLK_SYSCALL_FAIL"
    const TAG_OK: u64 = 14;
    const TAG_FAIL: u64 = 16;
    let (tag, word1) = if content_matches {
        (TAG_OK, WORD1_OK)
    } else {
        (TAG_FAIL, WORD1_FAIL)
    };

    unsafe {
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_SEND,
            in("rdi") 0u64, // CONSOLE_CAP
            in("rsi") tag,
            in("rdx") WORD0,
            in("r10") word1,
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
