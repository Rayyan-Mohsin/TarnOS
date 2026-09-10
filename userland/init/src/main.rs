//! TarnOS's first usermode process.
//!
//! Sends a greeting to the console server over capability slot 0 (the
//! well-known [`CONSOLE_CAP`] every process is seeded with at creation)
//! — the end-to-end proof that boot, the scheduler, the syscall ABI, and
//! rendezvous IPC all work together, not just each in isolation.
//!
//! Then (Milestone 3) does the same proof for dynamic process creation:
//! spawns `echo-child` — a process boot code never mentions at all —
//! grants it a capability of its own choosing (rights it holds but the
//! child starts with none of), releases it, and completes a genuine
//! round-trip rendezvous with it. None of this is boot-choreographed
//! the way the console greeting above is.
#![no_std]
#![no_main]

use tarnos_abi::{CapIndex, Message, Rights, CHILD_LINK_CAP, CONSOLE_CAP};
use tarnos_rt::syscall;

tarnos_rt::entry_point!(main);

/// The slot `echo-child` expects its link capability at in its own
/// (otherwise empty) capability table — this process's choice as the
/// granter, not a kernel-wide constant.
const CHILD_PARENT_LINK_CAP: CapIndex = CapIndex(0);

fn main() -> ! {
    // Must fit within a Message's 32-byte inline capacity (see
    // tarnos_abi::MESSAGE_INLINE_WORDS) — this milestone only supports
    // inline messages, no out-of-line payloads yet.
    let greeting = Message::from_str_lossy("Hello from TarnOS userspace!");
    let _ = syscall::sys_send(CONSOLE_CAP, greeting);

    let child_pid = syscall::sys_spawn("echo-child").expect("spawn failed");
    syscall::sys_grant(
        child_pid,
        CHILD_LINK_CAP,
        CHILD_PARENT_LINK_CAP,
        Rights::SEND | Rights::RECV,
    )
    .expect("grant failed");
    syscall::sys_process_start(child_pid).expect("process start failed");

    syscall::sys_send(CHILD_LINK_CAP, Message::from_str_lossy("ping")).expect("send to child failed");
    let reply = syscall::sys_recv(CHILD_LINK_CAP).expect("recv from child failed");
    let mut buf = [0u8; tarnos_abi::MESSAGE_INLINE_WORDS * 8];
    let text = reply.as_str_lossy(&mut buf);

    let mut report_buf = [0u8; 32];
    let report = format_child_reply(text, &mut report_buf);
    let _ = syscall::sys_send(CONSOLE_CAP, Message::from_str_lossy(report));

    syscall::sys_exit(0);
}

/// Builds `"child replied: <text>"` in a fixed, no-alloc buffer — this
/// process has no heap (see `tarnos-rt`'s doc comments), so a `format!`
/// isn't available.
fn format_child_reply<'a>(text: &str, buf: &'a mut [u8; 32]) -> &'a str {
    const PREFIX: &[u8] = b"child replied: ";
    let mut len = PREFIX.len();
    buf[..len].copy_from_slice(PREFIX);
    let remaining = buf.len() - len;
    let text_bytes = text.as_bytes();
    let take = core::cmp::min(text_bytes.len(), remaining);
    buf[len..len + take].copy_from_slice(&text_bytes[..take]);
    len += take;
    core::str::from_utf8(&buf[..len]).unwrap_or("child replied: <invalid utf-8>")
}
