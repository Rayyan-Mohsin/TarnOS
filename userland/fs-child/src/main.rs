//! Milestone 12 Phase 5's userland test fixture: reads a known file via
//! the real `SYS_FILE_READ` syscall (through `tarnos-rt`'s own wrapper,
//! not a raw `asm!` block) and reports whether the content matches.
//! `FS_CAP`/`CONSOLE_CAP` are seeded directly into this process's own
//! table by `main.rs`'s `fat-fixture-test` boot block, which spawns
//! this process the same way it spawns `init` itself
//! (`Process::from_elf`, not `SYS_SPAWN`) -- mirrors `block-child`'s own
//! Milestone 11 Phase 5 precedent exactly. Unlike
//! `kernel::milestone12_tests::fat_boundary_process` (a kernel function
//! copied into a dummy process's pages), this is a real, independently
//! linked ELF binary with no constraint against ordinary Rust code — a
//! ring-3, kernel-external counterpart to that same check.
#![no_std]
#![no_main]

use tarnos_abi::{Message, CONSOLE_CAP, FS_CAP};
use tarnos_rt::syscall;

tarnos_rt::entry_point!(main);

/// The same known test payload every other Phase 2-4 check already
/// confirms -- reused here rather than a second, made-up expectation.
const EXPECTED: &[u8] = b"TarnOS Milestone 12 FAT12 smoke test payload.\n";

fn main() -> ! {
    let mut buf = [0u8; 64];
    let content_matches = syscall::sys_file_read(FS_CAP, "HELLO.TXT", &mut buf)
        .map(|n| &buf[..n] == EXPECTED)
        .unwrap_or(false);

    let report = if content_matches {
        "FS_FIXTURE_OK"
    } else {
        "FS_FIXTURE_FAIL"
    };
    let _ = syscall::sys_send(CONSOLE_CAP, Message::from_str_lossy(report));
    syscall::sys_exit(if content_matches { 0 } else { 1 });
}
