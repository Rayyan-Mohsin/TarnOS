//! Milestone 5's `sys_sbrk`/heap proof: builds a `Vec<u64>` large enough
//! to force several `sys_sbrk` growths (not just one lucky allocation),
//! verifies every value it just wrote is still intact, and exits `0` on
//! success or `1` on any mismatch — verified by the parent's `SYS_WAIT`
//! reading that exit code, the same pattern `exit-code-child` already
//! established in Milestone 4. Needs no capabilities: it never does IPC.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use tarnos_rt::syscall;

tarnos_rt::entry_point!(main);

/// 64K `u64`s = 512 KiB — well past `tarnos_rt::heap`'s 16 KiB
/// `MIN_GROW_BYTES`, so this forces dozens of separate `sys_sbrk`
/// growths rather than a single one.
const COUNT: u64 = 64 * 1024;

fn main() -> ! {
    let mut values: Vec<u64> = Vec::new();
    for i in 0..COUNT {
        values.push(i);
    }

    let ok = values.len() as u64 == COUNT
        && values.iter().enumerate().all(|(i, &v)| v == i as u64);

    syscall::sys_exit(if ok { 0 } else { 1 });
}
