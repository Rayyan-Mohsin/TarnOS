//! Shared syscall ABI contract between the TarnOS kernel and userland.
//!
//! This crate is the single source of truth for the boundary between ring 0
//! and ring 3: syscall numbers, the wire shape of an IPC `Message`, capability
//! indices, and error codes. Both `tarnos-kernel` and userland binaries (via
//! `tarnos-rt`) depend on it, so the two sides of the syscall trampoline
//! cannot drift out of sync with each other.
#![no_std]

/// `sys_yield()` — voluntarily give up the remaining timeslice.
pub const SYS_YIELD: u64 = 0;
/// `sys_send(cap, tag, w0, w1, w2, w3)` — rendezvous-send a `Message` on a capability.
pub const SYS_SEND: u64 = 1;
/// `sys_recv(cap)` — rendezvous-receive a `Message` on a capability.
pub const SYS_RECV: u64 = 2;
/// `sys_exit(code)` — terminate the calling process.
pub const SYS_EXIT: u64 = 3;
/// `sys_spawn(name_lo, name_hi, name_len)` — create a new process from a
/// boot-shipped program named `name`, `Suspended` (not yet scheduled),
/// with the caller recorded as its parent. Returns the new `Pid`.
pub const SYS_SPAWN: u64 = 4;
/// `sys_grant(target_pid, src_cap, dest_cap, rights)` — clones the
/// capability at `src_cap` in the caller's own table into `dest_cap` in
/// `target_pid`'s table, narrowed to `rights` (which must be a subset of
/// what the caller holds). Only permitted while `target_pid` is a
/// `Suspended` child of the caller.
pub const SYS_GRANT: u64 = 5;
/// `sys_process_start(target_pid)` — releases a `Suspended` child of the
/// caller into the scheduler's ready queue. Once started, the child is
/// an ordinary independent process.
pub const SYS_PROCESS_START: u64 = 6;
/// `sys_wait(target_pid)` — blocks until `target_pid` (a child of the
/// caller, by `Pid`, not restricted to `Suspended`) exits, then returns
/// its [`ExitStatus`]. If `target_pid` already exited before this call,
/// returns immediately instead of blocking.
pub const SYS_WAIT: u64 = 7;
/// `sys_kill(target_pid)` — immediately terminates `target_pid`, a
/// child of the caller, regardless of its current state (`Ready`,
/// `Blocked`, or `Suspended`). Only permitted against the caller's own
/// child — the same structural-authority model `SYS_GRANT`/
/// `SYS_PROCESS_START` already use, extended to cover a child's whole
/// lifetime rather than only its `Suspended` window.
pub const SYS_KILL: u64 = 8;

/// An index into the *calling process's own* capability table.
///
/// Capabilities, not global handles, are how objects are named: a process
/// can only address an endpoint (or, later, a memory/IRQ object) that the
/// kernel has explicitly placed in one of its capability slots. There is no
/// global, guessable, or forgeable namespace.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CapIndex(pub u32);

/// The capability slot every process is seeded with at creation time,
/// granting send rights to the console server's endpoint. Analogous to a
/// seL4 root task's kernel-populated initial CSpace slot.
pub const CONSOLE_CAP: CapIndex = CapIndex(0);

/// `init`'s second boot-seeded capability: `SEND | RECV` on a fresh
/// endpoint reserved for talking to whatever child it spawns. Boot code
/// only ever seeds `init` with `SEND` on [`CONSOLE_CAP`] — without this
/// second slot `init` would hold no `RECV` right to grant a spawned
/// child in the first place. A spawned child itself starts with an
/// *empty* capability table; it receives whatever its parent grants it
/// at whatever index the parent chooses, which need not be this one.
pub const CHILD_LINK_CAP: CapIndex = CapIndex(1);

/// Maximum number of inline `u64` payload words carried by a `Message`.
///
/// Small messages are passed entirely in registers on both `send` and
/// `recv`, so there is no user-pointer to validate for the common case this
/// milestone exercises. Larger payloads are the designed extension point of
/// [`MessagePayload::OutOfLine`] inside the kernel, not of this wire type.
pub const MESSAGE_INLINE_WORDS: usize = 4;

/// The wire format of an IPC message: a caller-defined `tag` plus up to
/// [`MESSAGE_INLINE_WORDS`] inline `u64` words, passed entirely in registers.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Message {
    pub tag: u64,
    pub words: [u64; MESSAGE_INLINE_WORDS],
}

impl Message {
    pub const fn new(tag: u64, words: [u64; MESSAGE_INLINE_WORDS]) -> Self {
        Self { tag, words }
    }

    /// Packs up to `MESSAGE_INLINE_WORDS * 8` bytes of a UTF-8 string into a
    /// message's inline words, little-endian, zero-padded. `tag` is set to
    /// the byte length so the receiver knows where the text ends.
    ///
    /// Truncates silently if `s` is longer than the inline capacity — this
    /// milestone's demo strings fit comfortably; a real bounded-length API
    /// belongs on top of [`MessagePayload::OutOfLine`], not here.
    pub fn from_str_lossy(s: &str) -> Self {
        let bytes = s.as_bytes();
        let cap = MESSAGE_INLINE_WORDS * 8;
        let len = core::cmp::min(bytes.len(), cap);
        let mut buf = [0u8; MESSAGE_INLINE_WORDS * 8];
        buf[..len].copy_from_slice(&bytes[..len]);
        let mut words = [0u64; MESSAGE_INLINE_WORDS];
        for (i, word) in words.iter_mut().enumerate() {
            let start = i * 8;
            *word = u64::from_le_bytes(buf[start..start + 8].try_into().unwrap());
        }
        Self {
            tag: len as u64,
            words,
        }
    }

    /// Inverse of [`Message::from_str_lossy`]: reinterprets the inline words
    /// as `tag` bytes of UTF-8, lossily replacing invalid sequences.
    pub fn as_str_lossy<'a>(&self, buf: &'a mut [u8; MESSAGE_INLINE_WORDS * 8]) -> &'a str {
        for (i, word) in self.words.iter().enumerate() {
            buf[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        let len = core::cmp::min(self.tag as usize, buf.len());
        core::str::from_utf8(&buf[..len]).unwrap_or("<invalid utf-8>")
    }
}

/// How a process's execution ended, reported to its parent via
/// `sys_wait`. Lives here, not just inside the kernel, because it's
/// part of `sys_wait`'s return-value wire contract — a process needs to
/// be able to decode what it's handed back, the same way it decodes a
/// `SyscallError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// Voluntarily called `sys_exit(code)`.
    Exited(i32),
    /// Killed by a CPU exception it caused (see `arch::x86_64::idt`'s
    /// CPL-3 fault handlers).
    Faulted,
    /// Terminated by its parent's `sys_kill`.
    Killed,
}

impl ExitStatus {
    /// Packs into `sys_wait`'s two return registers, `(kind, code)` —
    /// `code` is only meaningful when `kind == 0`.
    pub const fn to_regs(self) -> (u64, u64) {
        match self {
            ExitStatus::Exited(code) => (0, code as u32 as u64),
            ExitStatus::Faulted => (1, 0),
            ExitStatus::Killed => (2, 0),
        }
    }

    /// Inverse of [`ExitStatus::to_regs`]. An unrecognized `kind` maps
    /// to `Killed` rather than panicking, for the same forward-
    /// compatibility reason [`SyscallError::from_retval`] does.
    pub fn from_regs(kind: u64, code: u64) -> Self {
        match kind {
            0 => ExitStatus::Exited(code as u32 as i32),
            1 => ExitStatus::Faulted,
            _ => ExitStatus::Killed,
        }
    }
}

bitflags::bitflags! {
    /// What a capability slot permits. Lives here, not in `tarnos-kcore`,
    /// because `sys_grant` makes it part of the wire contract between
    /// kernel and userland — a process must be able to *express* which
    /// rights it's requesting a grant with, the same way `Message` and
    /// `SyscallError` are shared wire types. `tarnos-kcore::captable`
    /// re-exports this rather than defining its own copy.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Rights: u8 {
        const SEND = 0b01;
        const RECV = 0b10;
    }
}

/// Longest program name `sys_spawn` accepts, packed into two `u64`
/// registers alongside a length — enough for every name this milestone's
/// fixed, boot-shipped set of spawnable programs uses. Not a general
/// bounded-string mechanism (there is no user-pointer validation
/// anywhere in this kernel yet, by design — see
/// `docs/adr/0003-ipc-message-format.md`); this mirrors `Message`'s own
/// register-packing idiom rather than introducing a new one.
pub const PROGRAM_NAME_MAX: usize = 16;

/// Packs a program name into `(lo, hi, len)` for `sys_spawn`'s three
/// register arguments, little-endian, truncated (not just padded) to
/// [`PROGRAM_NAME_MAX`] bytes — mirrors [`Message::from_str_lossy`].
pub fn pack_program_name(name: &str) -> (u64, u64, u64) {
    let bytes = name.as_bytes();
    let len = core::cmp::min(bytes.len(), PROGRAM_NAME_MAX);
    let mut buf = [0u8; PROGRAM_NAME_MAX];
    buf[..len].copy_from_slice(&bytes[..len]);
    let lo = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let hi = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    (lo, hi, len as u64)
}

/// Inverse of [`pack_program_name`]: reinterprets `lo`/`hi` as `len`
/// bytes of UTF-8, lossily replacing invalid sequences — mirrors
/// [`Message::as_str_lossy`].
pub fn unpack_program_name(lo: u64, hi: u64, len: u64, buf: &mut [u8; PROGRAM_NAME_MAX]) -> &str {
    buf[0..8].copy_from_slice(&lo.to_le_bytes());
    buf[8..16].copy_from_slice(&hi.to_le_bytes());
    let len = core::cmp::min(len as usize, buf.len());
    core::str::from_utf8(&buf[..len]).unwrap_or("<invalid utf-8>")
}

/// Syscall error codes, returned as `-(code as i64)` in RAX so success
/// (`>= 0`) and failure are distinguishable with a single sign check, the
/// same convention Linux's x86_64 syscall ABI uses.
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyscallError {
    /// The syscall number in RAX is not recognized.
    NoSuchSyscall = 1,
    /// The `CapIndex` does not name a live slot in the caller's table.
    BadCapability = 2,
    /// The capability's rights do not permit the requested operation.
    PermissionDenied = 3,
    /// A bounded kernel resource the operation needed is already fully
    /// committed — e.g. every possible process is already queued
    /// waiting on the same IPC endpoint. Reported as a real error
    /// instead of blocking, since blocking here would mean waiting with
    /// no way for anything to ever wake the caller.
    ResourceExhausted = 4,
    /// `sys_spawn`'s name did not match any boot-shipped program.
    NoSuchProgram = 5,
    /// `sys_grant`/`sys_process_start`'s `target_pid` does not name a
    /// `Suspended` child of the caller — either it isn't a child at all,
    /// or it already left the `Suspended` window (already started).
    InvalidTarget = 6,
    /// `sys_spawn` found the named program but could not construct a
    /// process from it (ELF load failure or the process table is full).
    SpawnFailed = 7,
}

impl SyscallError {
    /// Encode as the raw negative return value placed in RAX.
    pub const fn as_retval(self) -> i64 {
        -(self as u64 as i64)
    }

    /// Decodes a negative syscall return value back into an error.
    /// `retval` must be `< 0`; an unrecognized code maps to
    /// [`SyscallError::NoSuchSyscall`] rather than panicking, since a
    /// future kernel might return codes this build of `tarnos-abi`
    /// doesn't know about yet.
    pub fn from_retval(retval: i64) -> Self {
        match (-retval) as u64 {
            2 => SyscallError::BadCapability,
            3 => SyscallError::PermissionDenied,
            4 => SyscallError::ResourceExhausted,
            5 => SyscallError::NoSuchProgram,
            6 => SyscallError::InvalidTarget,
            7 => SyscallError::SpawnFailed,
            _ => SyscallError::NoSuchSyscall,
        }
    }
}

/// Which ABI a process's syscalls are dispatched against.
///
/// TarnOS-native syscalls (this milestone) and a future translated Linux
/// syscall table are meant to share the same trap entry/exit trampoline
/// (`arch::x86_64::syscall`) and only swap the dispatch table selected by
/// this tag — see `docs/adr/0004-posix-abi-seam.md`. Unused this milestone;
/// every process is created as `TarnosNative`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbiKind {
    TarnosNative,
    LinuxCompat,
}
