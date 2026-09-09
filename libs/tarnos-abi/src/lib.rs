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
}

impl SyscallError {
    /// Encode as the raw negative return value placed in RAX.
    pub const fn as_retval(self) -> i64 {
        -(self as u64 as i64)
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
