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
//!
//! Then (Milestone 12 Phase 3) probes `FS_CAP` — seeded directly into
//! this process's own capability table by boot code, not granted from
//! anywhere, whenever `fs::fat::mount_root` found a real FAT12 volume
//! this boot. Tolerant of absence: most scenarios attach no disk at
//! all, or a disk that isn't a valid FAT12 volume, and a missing
//! `FS_CAP` reads back as `SyscallError::BadCapability`, which is
//! treated as "no filesystem this boot" and silently skipped, not an
//! error to report.
#![no_std]
#![no_main]

use tarnos_abi::{CapIndex, Message, Rights, SyscallError, CHILD_LINK_CAP, CONSOLE_CAP, FS_CAP};
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

    probe_filesystem();

    syscall::sys_exit(0);
}

/// The same known test payload `main.rs`'s own `fat-fs-test`-gated
/// kernel-internal smoke test (Milestone 12 Phase 2) already checks --
/// reused here rather than a second, made-up expectation, so both
/// checks confirm the exact same real content, reached two different
/// ways (directly from kernel code vs. through a real syscall from this
/// process).
const HELLO_EXPECTED: &[u8] = b"TarnOS Milestone 12 FAT12 smoke test payload.\n";
const BIGFILE_LEN: usize = 3000;

/// Reads both of `xtask test-fat-parsing`'s own known test files through
/// `SYS_FILE_READ` and reports `FS_SYSCALL_OK`/`FS_SYSCALL_FAIL` over
/// the console -- silently does nothing if `FS_CAP` isn't a live
/// capability at all (see this file's own module doc comment for why
/// that's an ordinary outcome, not a bug). `BIGFILE.TXT` spans several
/// FAT12 clusters, so this also proves the same real cluster-chain walk
/// Phase 2 already confirmed kernel-side is reachable through the real
/// syscall/capability surface, from genuine ring-3 code.
fn probe_filesystem() {
    let mut hello_buf = [0u8; 64];
    let hello_ok = match syscall::sys_file_read(FS_CAP, "HELLO.TXT", &mut hello_buf) {
        Ok(n) => &hello_buf[..n] == HELLO_EXPECTED,
        Err(SyscallError::BadCapability) => return,
        Err(_) => false,
    };

    let mut bigfile_buf = [0u8; BIGFILE_LEN];
    let bigfile_ok = match syscall::sys_file_read(FS_CAP, "BIGFILE.TXT", &mut bigfile_buf) {
        Ok(n) => {
            n == BIGFILE_LEN
                && bigfile_buf.iter().enumerate().all(|(i, &b)| b == (i % 256) as u8)
        }
        Err(_) => false,
    };

    let report = if hello_ok && bigfile_ok {
        "FS_SYSCALL_OK"
    } else {
        "FS_SYSCALL_FAIL"
    };
    let _ = syscall::sys_send(CONSOLE_CAP, Message::from_str_lossy(report));
}

/// Builds `"child replied: <text>"` in a fixed, no-alloc buffer. `init`
/// doesn't pull in `extern crate alloc` at all (unlike `heap-child`,
/// which exists specifically to exercise `tarnos-rt`'s heap) — a
/// `format!` call needs `alloc::string::String`, so it isn't available
/// here without that import, even though `tarnos-rt` itself always
/// wires up a working global allocator for any binary that links it.
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
