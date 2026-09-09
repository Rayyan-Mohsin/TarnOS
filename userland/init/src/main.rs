//! TarnOS's first usermode process.
//!
//! Sends a greeting to the console server over capability slot 0 (the
//! well-known [`CONSOLE_CAP`] every process is seeded with at creation)
//! and exits — the end-to-end proof that boot, the scheduler, the
//! syscall ABI, and rendezvous IPC all work together, not just each in
//! isolation.
#![no_std]
#![no_main]

use tarnos_abi::{Message, CONSOLE_CAP};
use tarnos_rt::syscall;

tarnos_rt::entry_point!(main);

fn main() -> ! {
    let greeting = Message::from_str_lossy("Hello from userspace, TarnOS is alive!");
    let _ = syscall::sys_send(CONSOLE_CAP, greeting);
    syscall::sys_exit(0);
}
