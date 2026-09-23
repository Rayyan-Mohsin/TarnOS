//! Ring-3 entry points for Milestone 12 Phase 5's `SYS_FILE_READ`
//! adversarial boundary test (see `xtask`'s `test-fat-boundary`
//! command).
//!
//! Same constraints as `milestone11_tests`'s own dummy processes: stays
//! within the two pages `Process::new_dummy` copies, every multi-byte
//! value is a `const` baked into the instruction stream, and there are
//! no Rust-level function calls anywhere in this body (a real call
//! would jump outside the copied page(s), into whatever kernel code
//! happened to compile next to it) -- every expected `SYS_FILE_READ`
//! result below is a raw literal for exactly that reason, not a call to
//! `SyscallError::as_retval()`.
#![cfg(feature = "fat-boundary-test")]
use core::arch::asm;

/// Milestone 12 Phase 5: adversarial boundary tests for `SYS_FILE_READ`
/// -- the same "prove the boundary is enforced, not just unexercised"
/// bar `milestone11_tests::block_boundary_process` already holds
/// itself to. `main.rs`'s own `fat-boundary-test` boot block seeds this
/// process with `FS_CAP` (index 3) and `CONSOLE_CAP` (index 0), and
/// mounts a real FAT12 volume containing `HELLO.TXT` (46 bytes) before
/// spawning it; check 2 deliberately targets a *different*,
/// never-seeded index instead.
///
/// Four checks, each its own inline `asm!` block:
///
/// 1. A file name that doesn't exist on the volume — must fail with
///    `NoSuchFile` (`-12`).
/// 2. A capability index nothing was ever seeded into — must fail with
///    `BadCapability` (`-2`), not silently treated as "no rights."
/// 3. A canonical but entirely unmapped destination buffer — must fail
///    with `InvalidArgument` (`-8`) without ever touching the
///    filesystem, proving the buffer is validated *before* any I/O.
/// 4. A destination buffer larger than `HELLO.TXT`'s own size — must
///    *succeed*, returning exactly `46` (the file's real length), never
///    an error and never more bytes than the file actually has.
///
/// Reports `"FAT_BOUNDARY_OK"` (all four checks matched their expected
/// result) or `"FAT_BOUNDARY_FAIL"` on `CONSOLE_CAP`, then exits.
pub unsafe extern "C" fn fat_boundary_process() -> ! {
    const VALID_CAP: u64 = 3; // FS_CAP
    const MISSING_CAP: u64 = 5; // never seeded by main.rs's boot block

    // "HELLO.TXT" packed via the same lo/hi/len scheme
    // `tarnos_abi::pack_short_name` uses -- computed by hand here (byte
    // string literals, no function call) for the same reason every
    // other value in this file is a raw literal.
    const HELLO_LO: u64 = u64::from_le_bytes(*b"HELLO.TX");
    const HELLO_HI: u64 = u64::from_le_bytes(*b"T\0\0\0\0\0\0\0");
    const HELLO_LEN: u64 = 9;
    const HELLO_FILE_SIZE: i64 = 46; // len(b"TarnOS Milestone 12 FAT12 smoke test payload.\n")

    // "NOSUCH.TXT" -- deliberately absent from every FAT image this
    // test attaches.
    const NOSUCH_LO: u64 = u64::from_le_bytes(*b"NOSUCH.T");
    const NOSUCH_HI: u64 = u64::from_le_bytes(*b"XT\0\0\0\0\0\0");
    const NOSUCH_LEN: u64 = 10;

    // Canonical (bit 47 clear) but far above anything this kernel ever
    // maps for a process this small -- guaranteed unmapped without
    // needing to guess this process's own exact layout.
    const UNMAPPED_BUF: u64 = 0x0000_7000_0000_0000;
    const BUF_LEN: u64 = 128;

    const EXPECT_NO_SUCH_FILE: i64 = -12;
    const EXPECT_BAD_CAPABILITY: i64 = -2;
    const EXPECT_INVALID_ARGUMENT: i64 = -8;

    // `MaybeUninit`, not `[0u8; 128]` -- see
    // `milestone11_tests::block_read_syscall_process`'s own doc comment
    // for why a zero-initialized array this size risks a `memset` call
    // outside this process's own copied pages in an unoptimized debug
    // build.
    let mut buf: core::mem::MaybeUninit<[u8; BUF_LEN as usize]> = core::mem::MaybeUninit::uninit();
    let buf_ptr = buf.as_mut_ptr() as u64;
    let mut all_ok = true;

    let result1: i64;
    unsafe {
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_FILE_READ,
            in("rdi") VALID_CAP,
            in("rsi") NOSUCH_LO,
            in("rdx") NOSUCH_HI,
            in("r10") NOSUCH_LEN,
            in("r8") buf_ptr,
            in("r9") BUF_LEN,
            lateout("rax") result1,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    if result1 != EXPECT_NO_SUCH_FILE {
        all_ok = false;
    }

    let result2: i64;
    unsafe {
        asm!(
            "syscall",
            in("rax") tarnos_abi::SYS_FILE_READ,
            in("rdi") MISSING_CAP,
            in("rsi") HELLO_LO,
            in("rdx") HELLO_HI,
            in("r10") HELLO_LEN,
            in("r8") buf_ptr,
            in("r9") BUF_LEN,
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
            in("rax") tarnos_abi::SYS_FILE_READ,
            in("rdi") VALID_CAP,
            in("rsi") HELLO_LO,
            in("rdx") HELLO_HI,
            in("r10") HELLO_LEN,
            in("r8") UNMAPPED_BUF,
            in("r9") BUF_LEN,
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
            in("rax") tarnos_abi::SYS_FILE_READ,
            in("rdi") VALID_CAP,
            in("rsi") HELLO_LO,
            in("rdx") HELLO_HI,
            in("r10") HELLO_LEN,
            in("r8") buf_ptr,
            in("r9") BUF_LEN,
            lateout("rax") result4,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    if result4 != HELLO_FILE_SIZE {
        all_ok = false;
    }

    // Top-level `const`s selected by a runtime `if`, never a literal
    // evaluated inside the branch itself -- see
    // `milestone11_tests::block_read_syscall_process`'s own doc comment
    // for why.
    const WORD0: u64 = u64::from_le_bytes(*b"FAT_BOUN");
    const WORD1_OK: u64 = u64::from_le_bytes(*b"DARY_OK\0"); // "FAT_BOUNDARY_OK" (15 bytes)
    const WORD2_OK: u64 = 0;
    const TAG_OK: u64 = 15;
    const WORD1_FAIL: u64 = u64::from_le_bytes(*b"DARY_FAI");
    const WORD2_FAIL: u64 = u64::from_le_bytes(*b"L\0\0\0\0\0\0\0"); // "FAT_BOUNDARY_FAIL" (17 bytes)
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
