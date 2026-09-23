//! Ring-3 entry points for Milestone 11 Phase 4/5's `SYS_BLOCK_READ`
//! tests (see `xtask`'s `test-block-syscall`/`test-block-boundary`
//! commands).
//!
//! Same constraints as `milestone7_tests`/`milestone8_tests`'s dummy
//! processes: stays within the two pages `Process::new_dummy` copies,
//! every multi-byte value is a `const` baked into the instruction
//! stream, and there are no Rust-level function calls anywhere in this
//! body (a real call would jump outside the copied page(s), into
//! whatever kernel code happened to compile next to it) -- the read-back
//! content check below is a raw indexing loop for exactly that reason,
//! not an iterator adapter that could monomorphize into a separate,
//! not-copied function. `block_boundary_process` below hardcodes every
//! expected `SYS_BLOCK_READ` error as a raw negative-`i64` literal for
//! the same reason: calling `SyscallError::as_retval()`, even though
//! it's a trivial one-line `const fn`, is still a Rust-level function
//! call an unoptimized debug build has no obligation to inline.
#![cfg(any(feature = "block-syscall-test", feature = "block-boundary-test"))]
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
#[cfg(feature = "block-syscall-test")]
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

/// Milestone 11 Phase 5: adversarial boundary tests for `SYS_BLOCK_READ`
/// -- the same "prove the boundary is enforced, not just unexercised"
/// bar every prior milestone's own boundary tests already hold to (see
/// `xtask`'s `test-block-boundary` command). `main.rs`'s own
/// `block-boundary-test` boot block seeds this process with `BLOCK_CAP`
/// (index 2) and `CONSOLE_CAP` (index 0) -- exactly like
/// `block_read_syscall_process` above -- so every check below that's
/// meant to hold a valid capability genuinely does; check 2 deliberately
/// targets a *different*, never-seeded index instead.
///
/// Four checks, each its own inline `asm!` block:
///
/// 1. An LBA past the disk image's own capacity (64 sectors --
///    `xtask::create_test_disk_image`'s own fixed size) — must fail
///    with `IoOutOfRange` (`-10`).
/// 2. A capability index nothing was ever seeded into — must fail with
///    `BadCapability` (`-2`), not silently treated as "no rights."
/// 3. `sector_count == 0` — must fail with `InvalidArgument` (`-8`)
///    without ever touching the device.
/// 4. A canonical but entirely unmapped buffer address — must fail with
///    `InvalidArgument` (`-8`) without ever touching the device,
///    proving the buffer is validated *before* any I/O, not merely
///    that a bad write happens not to crash anything.
///
/// Reports `"BLK_BOUNDARY_OK"` (all four checks matched their expected
/// result) or `"BLK_BOUNDARY_FAIL"` on `CONSOLE_CAP`, then exits.
#[cfg(feature = "block-boundary-test")]
pub unsafe extern "C" fn block_boundary_process() -> ! {
    const VALID_CAP: u64 = 2; // BLOCK_CAP
    const MISSING_CAP: u64 = 5; // never seeded by main.rs's boot block
    const VALID_LBA: u64 = 2;
    const OUT_OF_RANGE_LBA: u64 = 1000; // past the 64-sector test disk
    // Canonical (bit 47 clear) but far above anything this kernel ever
    // maps for a process this small (ELF segments, a 16 KiB stack, an
    // as-yet-ungrown sbrk heap) -- guaranteed unmapped without needing
    // to guess this process's own exact layout.
    const UNMAPPED_BUF: u64 = 0x0000_7000_0000_0000;

    const EXPECT_IO_OUT_OF_RANGE: i64 = -10;
    const EXPECT_BAD_CAPABILITY: i64 = -2;
    const EXPECT_INVALID_ARGUMENT: i64 = -8;

    let mut buf: core::mem::MaybeUninit<[u8; 512]> = core::mem::MaybeUninit::uninit();
    let buf_ptr = buf.as_mut_ptr() as u64;
    let mut all_ok = true;

    let result1: i64;
    unsafe {
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_BLOCK_READ,
            in("rdi") VALID_CAP,
            in("rsi") OUT_OF_RANGE_LBA,
            in("rdx") buf_ptr,
            in("r10") 1u64,
            in("r8") 0u64,
            in("r9") 0u64,
            lateout("rax") result1,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    if result1 != EXPECT_IO_OUT_OF_RANGE {
        all_ok = false;
    }

    let result2: i64;
    unsafe {
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_BLOCK_READ,
            in("rdi") MISSING_CAP,
            in("rsi") VALID_LBA,
            in("rdx") buf_ptr,
            in("r10") 1u64,
            in("r8") 0u64,
            in("r9") 0u64,
            lateout("rax") result2,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    if result2 != EXPECT_BAD_CAPABILITY {
        all_ok = false;
    }

    let result3: i64;
    unsafe {
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_BLOCK_READ,
            in("rdi") VALID_CAP,
            in("rsi") VALID_LBA,
            in("rdx") buf_ptr,
            in("r10") 0u64,
            in("r8") 0u64,
            in("r9") 0u64,
            lateout("rax") result3,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    if result3 != EXPECT_INVALID_ARGUMENT {
        all_ok = false;
    }

    let result4: i64;
    unsafe {
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_BLOCK_READ,
            in("rdi") VALID_CAP,
            in("rsi") VALID_LBA,
            in("rdx") UNMAPPED_BUF,
            in("r10") 1u64,
            in("r8") 0u64,
            in("r9") 0u64,
            lateout("rax") result4,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    if result4 != EXPECT_INVALID_ARGUMENT {
        all_ok = false;
    }

    // Top-level `const`s selected by a runtime `if`, never a literal
    // evaluated inside the branch itself -- see
    // `block_read_syscall_process`'s own doc comment for why.
    const WORD0: u64 = u64::from_le_bytes(*b"BLK_BOUN");
    const WORD1_OK: u64 = u64::from_le_bytes(*b"DARY_OK\0"); // "BLK_BOUNDARY_OK" (15 bytes)
    const WORD2_OK: u64 = 0;
    const TAG_OK: u64 = 15;
    const WORD1_FAIL: u64 = u64::from_le_bytes(*b"DARY_FAI");
    const WORD2_FAIL: u64 = u64::from_le_bytes(*b"L\0\0\0\0\0\0\0"); // "BLK_BOUNDARY_FAIL" (17 bytes)
    const TAG_FAIL: u64 = 17;
    let (tag, word1, word2) = if all_ok {
        (TAG_OK, WORD1_OK, WORD2_OK)
    } else {
        (TAG_FAIL, WORD1_FAIL, WORD2_FAIL)
    };

    unsafe {
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_SEND,
            in("rdi") 0u64, // CONSOLE_CAP
            in("rsi") tag,
            in("rdx") WORD0,
            in("r10") word1,
            in("r8") word2,
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
