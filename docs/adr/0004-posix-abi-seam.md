# 0004: POSIX/Linux ABI Compatibility Seam

## Status

Accepted (rationale only — no dispatch logic implemented this milestone).
The one concrete artifact is `tarnos_abi::AbiKind`
(`libs/tarnos-abi/src/lib.rs`), added but unused: every process is
created as `AbiKind::TarnosNative`, and nothing reads the tag yet.

## Context

TarnOS's stated priorities rank a POSIX/Linux ABI compatibility strategy
above driver framework and async concurrency, but below microkernel
isolation and zero-trust memory. This milestone builds none of the actual
POSIX shim — no `write`/`openat`/`read`/`mmap`/`clone` translation exists
yet. What this milestone does do is make sure nothing decided so far
forecloses that work or forces it into a more expensive shape later.

The risk being designed against: if the native syscall trap
entry/exit convention were chosen without POSIX in mind, adding Linux
compatibility later could mean building a second, separate trap
trampoline (its own `SYSCALL`/`SYSRET` handling, its own register-save
path) — doubling the audited `unsafe` surface for no architectural
reason, since a CPU only has one `SYSCALL` instruction and one entry
vector regardless of which ABI's syscall numbers a given process happens
to use.

## Decision

**Linux-shaped calling convention, chosen now, for native syscalls.**
TarnOS's own native syscalls (`SYS_YIELD`, `SYS_SEND`, `SYS_RECV`,
`SYS_EXIT` today) already use `SYSCALL`/`SYSRET` with Linux's x86_64
register convention — RAX carries the syscall number, RDI/RSI/RDX/R10/
R8/R9 carry up to six arguments — even though TarnOS's own syscall table
has nothing to do with Linux's. This is deliberate reuse of a calling
convention, not "basing TarnOS on Linux": the actual syscall numbers,
argument meanings, and error codes (`SyscallError`) are entirely
TarnOS's own. The payoff is that a future Linux-compatible dispatch table
can share the exact same trap entry/exit code
(`arch::x86_64::syscall`) instead of needing its own.

**A per-process ABI tag, added but not wired up.** `tarnos_abi::AbiKind`
is a two-variant enum (`TarnosNative | LinuxCompat`) intended to live on
`Process` and select, at syscall-dispatch time, which table a given
process's syscalls are looked up against. It exists today purely as a
placeholder — a `Process` doesn't even carry the field yet — so that when
a POSIX shim milestone actually arrives, the dispatch-table-selection
point is already named and doesn't require redesigning the syscall entry
path itself.

**In-kernel translation, not a separate shim process.** The intended
design (not yet built) is for recognized Linux syscalls to translate
in-kernel into native `send`/`recv` calls against native endpoints and
capabilities — e.g. a Linux `write(fd, buf, len)` becoming a native
`send` on whatever capability `fd` maps to — rather than round-tripping
every POSIX call through an extra IPC hop out to a separate userspace
translation process. The latter is the more "purely microkernel" design
in the abstract, but it doubles the number of context switches every
POSIX syscall costs (trap in, IPC out to the shim, IPC back, return) for
a compatibility path that exists specifically to make TarnOS *not*
slower than Linux for ported programs. A POSIX fd table is expected to
become "an array of `CapIndex` plus POSIX-specific metadata (offset,
flags)" layered on top of the existing capability model, rather than a
competing namespace that needs its own rights-checking logic.

## Consequences

- The trap trampoline in `arch::x86_64::syscall` must stay generic enough
  that a future dispatch-table swap (native vs. Linux-compat) is the only
  thing that changes — any future change to that file should be checked
  against "does this assume TarnOS-native semantics in a way that would
  break a Linux-compat dispatch table sharing this same entry path."
- `SyscallError`'s encoding (`-(code as i64)` in RAX, success is `>= 0`)
  was chosen to match Linux's x86_64 convention for the same reason —
  a future Linux dispatch table returns errors the exact same way a
  translated glibc/musl `errno` path expects, with no extra translation
  layer for the sign convention itself.
- None of the actual translation logic, fd-table-over-capabilities
  design, or `clone`/`mmap` semantics have been designed in detail — this
  ADR records only the seam (calling convention + dispatch tag), not a
  plan for the shim's internals. That remains a substantial, mostly
  unstarted body of work.
- `AbiKind` living unused in `tarnos-abi` with no field on `Process`
  referencing it is intentionally speculative — if a POSIX milestone
  doesn't materialize for a long time, this should either get wired up
  or be revisited, rather than accumulating more unused placeholders on
  top of it.
