//! Shared syscall ABI contract between the TarnOS kernel and userland.
//!
//! This crate is the single source of truth for the boundary between ring 0
//! and ring 3: syscall numbers, the wire shape of an IPC `Message`, capability
//! indices, and error codes. Both `tarnos-kernel` and userland binaries (via
//! `tarnos-rt`) depend on it, so the two sides of the syscall trampoline
//! cannot drift out of sync with each other.
//!
//! `#![cfg_attr(not(test), no_std)]`, not a bare `#![no_std]` — mirrors
//! `tarnos-kcore`'s own pattern (see that crate's `lib.rs` doc comment):
//! this crate is genuinely `no_std` in every real build (the kernel and
//! every userland binary that links it), but compiles as an ordinary
//! `std` crate under `cargo test`, which is what lets the standard
//! `#[test]`/`proptest!` harness run at all.
#![cfg_attr(not(test), no_std)]

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
/// `sys_sbrk(increment)` — grows the caller's heap by `increment` bytes
/// (must be `>= 0` this milestone — see [`SyscallError::InvalidArgument`])
/// and returns the *previous* break address. `increment == 0` is a
/// side-effect-free query of the current break.
pub const SYS_SBRK: u64 = 9;
/// `sys_block_read(cap, lba, buf_ptr, sector_count)` — reads
/// `sector_count` whole 512-byte sectors starting at `lba` from the
/// block device named by `cap` into the caller's own buffer at
/// `buf_ptr..buf_ptr + sector_count * 512`. `cap` must hold
/// [`Rights::READ`]. This kernel's first syscall that writes through a
/// caller-supplied pointer rather than only register-passed words or a
/// kernel-chosen address (see [`Rights::READ`]'s own doc comment) —
/// every byte of the destination range is validated as present,
/// writable, user-accessible memory in the caller's own address space
/// before anything is read from the device; an invalid range fails with
/// [`SyscallError::InvalidArgument`] without touching the device at all.
pub const SYS_BLOCK_READ: u64 = 10;

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

/// The fixed, well-known capability index for [`Rights::READ`] on the
/// one virtio-blk device this milestone builds — analogous to
/// [`CONSOLE_CAP`]/[`CHILD_LINK_CAP`] (a process is meant to find it at
/// this index, never discover it dynamically), though which process(es)
/// boot code actually seeds it into is still settling: Phase 4's own
/// dummy-process syscall test seeds it directly into its own throwaway
/// process's table; wiring it into the real `init` process (for Phase
/// 5's userland fixture to receive via `SYS_GRANT`, the same path
/// [`CHILD_LINK_CAP`] enables for `echo-child`) is that phase's own
/// work, not done yet.
pub const BLOCK_CAP: CapIndex = CapIndex(2);

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
        const SEND = 0b001;
        const RECV = 0b010;
        /// Permits `sys_block_read` against a `KernelObjectRef::BlockDevice`
        /// capability slot — the first right guarding something other than
        /// IPC. Named `READ`, not e.g. `BLOCK_READ`: this crate's own
        /// `Rights` type is generic authority a capability slot carries,
        /// not tied to one kernel object kind (`SEND`/`RECV` aren't named
        /// `ENDPOINT_SEND`/`ENDPOINT_RECV` either), and "read" is the
        /// obviously-right word for what it permits regardless of which
        /// future object kind might also want to reuse it.
        const READ = 0b100;
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
    /// `sys_sbrk`'s `increment` is negative (shrinking is not supported
    /// this milestone), would overflow the break address, or would grow
    /// the heap past its fixed per-process ceiling.
    InvalidArgument = 8,
    /// A blocked `SYS_SEND`/`SYS_RECV` was woken because the endpoint's
    /// last remaining live holder of the complementary right (the only
    /// process that could ever have completed this rendezvous) exited or
    /// was killed while this call was still blocked — never returned for
    /// any other reason. Without this, that call would otherwise block
    /// forever: nothing else was ever going to send or receive on this
    /// endpoint again. See `docs/adr/0013`.
    PeerClosed = 9,
    /// `sys_block_read`'s `lba..lba + sector_count` range extends at or
    /// past the device's own reported capacity.
    IoOutOfRange = 10,
    /// `sys_block_read`'s underlying device reported failure completing
    /// an otherwise well-formed, in-range request.
    IoError = 11,
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
    ///
    /// Uses `unsigned_abs`, not a bare `(-retval) as u64`: negating
    /// `i64::MIN` directly overflows (there is no positive `i64`
    /// representation of `-i64::MIN`) and panics under debug overflow
    /// checks — a real, if never-triggered-by-a-real-kernel, gap a
    /// property test surfaced by exercising this function's full
    /// documented input domain ("any negative value"), not just the
    /// small set of codes `as_retval` actually produces.
    pub fn from_retval(retval: i64) -> Self {
        match retval.unsigned_abs() {
            2 => SyscallError::BadCapability,
            3 => SyscallError::PermissionDenied,
            4 => SyscallError::ResourceExhausted,
            5 => SyscallError::NoSuchProgram,
            6 => SyscallError::InvalidTarget,
            7 => SyscallError::SpawnFailed,
            8 => SyscallError::InvalidArgument,
            9 => SyscallError::PeerClosed,
            10 => SyscallError::IoOutOfRange,
            11 => SyscallError::IoError,
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

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// ASCII-only, at or under the inline capacity: the packed
        /// register triple round-trips to *exactly* the original string —
        /// no truncation, no lossy replacement, since every byte is both
        /// present and a valid UTF-8 boundary on its own.
        #[test]
        fn pack_program_name_round_trips_short_ascii(name in "[ -~]{0,16}") {
            let (lo, hi, len) = pack_program_name(&name);
            let mut buf = [0u8; PROGRAM_NAME_MAX];
            let out = unpack_program_name(lo, hi, len, &mut buf);
            prop_assert_eq!(out, name);
        }

        /// Longer than the inline capacity: truncated to exactly the
        /// first `PROGRAM_NAME_MAX` bytes — still exact (not lossy) since
        /// the input is pure ASCII, so any byte offset is a valid UTF-8
        /// boundary.
        #[test]
        fn pack_program_name_truncates_long_ascii(name in "[ -~]{17,64}") {
            let (lo, hi, len) = pack_program_name(&name);
            let mut buf = [0u8; PROGRAM_NAME_MAX];
            let out = unpack_program_name(lo, hi, len, &mut buf);
            prop_assert_eq!(out, &name[..PROGRAM_NAME_MAX]);
        }

        /// Arbitrary Unicode input (which can straddle a multi-byte
        /// codepoint right at the truncation boundary) must never panic —
        /// `unpack_program_name`'s own lossy fallback is exactly the
        /// mechanism that's supposed to handle that, not a `unwrap()`
        /// this could still panic through.
        #[test]
        fn pack_program_name_never_panics_on_arbitrary_unicode(name in ".{0,64}") {
            let (lo, hi, len) = pack_program_name(&name);
            let mut buf = [0u8; PROGRAM_NAME_MAX];
            let _ = unpack_program_name(lo, hi, len, &mut buf);
        }

        #[test]
        fn message_round_trips_short_ascii(s in "[ -~]{0,32}") {
            let msg = Message::from_str_lossy(&s);
            let mut buf = [0u8; MESSAGE_INLINE_WORDS * 8];
            prop_assert_eq!(msg.as_str_lossy(&mut buf), s);
        }

        #[test]
        fn message_truncates_long_ascii(s in "[ -~]{33,128}") {
            let msg = Message::from_str_lossy(&s);
            let mut buf = [0u8; MESSAGE_INLINE_WORDS * 8];
            prop_assert_eq!(msg.as_str_lossy(&mut buf), &s[..MESSAGE_INLINE_WORDS * 8]);
        }

        #[test]
        fn message_never_panics_on_arbitrary_unicode(s in ".{0,128}") {
            let msg = Message::from_str_lossy(&s);
            let mut buf = [0u8; MESSAGE_INLINE_WORDS * 8];
            let _ = msg.as_str_lossy(&mut buf);
        }

        /// `SyscallError::from_retval` must never panic on *any* negative
        /// value, including ones no current variant maps to — an
        /// unrecognized code is documented to fall back to
        /// `NoSuchSyscall`, forward-compatibility with a newer kernel
        /// this build of the ABI crate doesn't fully know about yet.
        #[test]
        fn syscall_error_from_retval_never_panics(retval in i64::MIN..0) {
            let _ = SyscallError::from_retval(retval);
        }

        #[test]
        fn exit_status_exited_round_trips_through_regs(code in any::<i32>()) {
            let status = ExitStatus::Exited(code);
            let (kind, regs_code) = status.to_regs();
            prop_assert_eq!(ExitStatus::from_regs(kind, regs_code), status);
        }

        /// `from_regs` must never panic on an arbitrary `(kind, code)`
        /// pair — an unrecognized `kind` is documented to map to
        /// `Killed` rather than panicking, the same forward-compatibility
        /// reasoning as `SyscallError::from_retval`.
        #[test]
        fn exit_status_from_regs_never_panics(kind in any::<u64>(), code in any::<u64>()) {
            let _ = ExitStatus::from_regs(kind, code);
        }
    }

    /// Every defined `SyscallError` variant round-trips through its own
    /// wire encoding — enumerated rather than randomly generated, since
    /// there are only a handful of fieldless variants and every one of
    /// them matters (a gap here would mean a future variant was added to
    /// the enum without updating `from_retval`'s match, silently aliasing
    /// it to `NoSuchSyscall`).
    #[test]
    fn syscall_error_round_trips_every_variant() {
        let variants = [
            SyscallError::NoSuchSyscall,
            SyscallError::BadCapability,
            SyscallError::PermissionDenied,
            SyscallError::ResourceExhausted,
            SyscallError::NoSuchProgram,
            SyscallError::InvalidTarget,
            SyscallError::SpawnFailed,
            SyscallError::InvalidArgument,
            SyscallError::PeerClosed,
            SyscallError::IoOutOfRange,
            SyscallError::IoError,
        ];
        for err in variants {
            assert_eq!(SyscallError::from_retval(err.as_retval()), err);
        }
    }

    #[test]
    fn exit_status_faulted_and_killed_round_trip() {
        assert_eq!(
            ExitStatus::from_regs(1, 0),
            ExitStatus::Faulted
        );
        let (kind, code) = ExitStatus::Faulted.to_regs();
        assert_eq!(ExitStatus::from_regs(kind, code), ExitStatus::Faulted);

        let (kind, code) = ExitStatus::Killed.to_regs();
        assert_eq!(ExitStatus::from_regs(kind, code), ExitStatus::Killed);
    }

    /// Deterministic regression pin for the `i64::MIN` overflow the
    /// `syscall_error_from_retval_never_panics` property test above is
    /// meant to catch, but only ever does probabilistically (random
    /// sampling isn't guaranteed to land on this one exact boundary
    /// value every run) — a fixed test names it explicitly so a future
    /// regression back to `(-retval) as u64` fails every run, not just
    /// the lucky ones.
    #[test]
    fn syscall_error_from_retval_handles_i64_min() {
        assert_eq!(SyscallError::from_retval(i64::MIN), SyscallError::NoSuchSyscall);
    }
}
