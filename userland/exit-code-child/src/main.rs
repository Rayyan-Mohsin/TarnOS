//! Milestone 4's `SYS_WAIT` proof: a process with no capabilities and no
//! purpose beyond exiting with a specific, checkable code. Deliberately
//! separate from `echo-child` — reusing it would conflate "the IPC
//! round trip worked" with "`SYS_WAIT` reported the right exit code,"
//! two different things `xtask test-wait-exit-code` wants to check in
//! isolation.
#![no_std]
#![no_main]

use tarnos_rt::syscall;

tarnos_rt::entry_point!(main);

fn main() -> ! {
    syscall::sys_exit(42);
}
