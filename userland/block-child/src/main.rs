//! Milestone 11 Phase 5's userland test fixture: reads a known sector
//! via the real `SYS_BLOCK_READ` syscall (through `tarnos-rt`'s own
//! wrapper, not a raw `asm!` block) and reports whether the content
//! matches. `BLOCK_CAP`/`CONSOLE_CAP` are seeded directly into this
//! process's own table by `main.rs`'s `block-fixture-test` boot block,
//! which spawns this process the same way it spawns `init` itself
//! (`Process::from_elf`, not `SYS_SPAWN`) — see that block's own doc
//! comment. Unlike `kernel::milestone11_tests::block_read_syscall_process`
//! (a kernel function copied into a dummy process's pages), this is a
//! real, independently linked ELF binary with no constraint against
//! ordinary Rust code — a ring-3, kernel-external counterpart to that
//! same check.
#![no_std]
#![no_main]

use tarnos_abi::{Message, BLOCK_CAP, CONSOLE_CAP};
use tarnos_rt::syscall;

tarnos_rt::entry_point!(main);

/// Matches `xtask::create_test_disk_image`'s own fixed convention: a
/// disk image with every byte zero except sector 2, which holds a
/// `0..=255`-repeating pattern.
const KNOWN_TEST_LBA: u64 = 2;

fn main() -> ! {
    let mut buf = [0u8; 512];
    let read_ok = syscall::sys_block_read(BLOCK_CAP, KNOWN_TEST_LBA, &mut buf, 1).is_ok();
    let content_matches =
        read_ok && buf.iter().enumerate().all(|(i, &b)| b == (i % 256) as u8);

    let report = if content_matches {
        "BLOCK_FIXTURE_OK"
    } else {
        "BLOCK_FIXTURE_FAIL"
    };
    let _ = syscall::sys_send(CONSOLE_CAP, Message::from_str_lossy(report));
    syscall::sys_exit(if content_matches { 0 } else { 1 });
}
