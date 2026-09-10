//! Milestone 3's proof that dynamic process creation and capability
//! transfer are real, not just demonstrated: unlike `init`, this
//! process is never mentioned by boot code at all. It exists only
//! because `init` called `SYS_SPAWN("echo-child")`, and it starts with
//! a completely *empty* capability table — the only reason it can talk
//! to anything is that its parent granted it a capability at
//! `CapIndex(0)` after creating it (see `SYS_GRANT`) and then released
//! it with `SYS_PROCESS_START`.
#![no_std]
#![no_main]

use tarnos_abi::{CapIndex, Message};
use tarnos_rt::syscall;

tarnos_rt::entry_point!(main);

/// The slot `init` grants its link endpoint into — this process's own
/// choice of index in its own (otherwise empty) capability table, not a
/// kernel-wide well-known constant like `CONSOLE_CAP`.
const PARENT_LINK_CAP: CapIndex = CapIndex(0);

fn main() -> ! {
    let ping = syscall::sys_recv(PARENT_LINK_CAP).unwrap_or_else(|_| syscall::sys_exit(1));
    let _ = ping;
    let pong = Message::from_str_lossy("pong");
    let _ = syscall::sys_send(PARENT_LINK_CAP, pong);
    syscall::sys_exit(0);
}
