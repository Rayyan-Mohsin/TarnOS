# 0003: Synchronous Rendezvous IPC and the Inline Message Format

## Status

Accepted. Implemented in `kernel/src/ipc/endpoint.rs`,
`kernel/src/ipc/message.rs`, `libs/tarnos-abi/src/lib.rs`.

## Context

A microkernel's IPC mechanism is not an implementation detail — it's the
one thing every cross-process interaction goes through, so its shape
constrains everything built on top of it (drivers-as-processes, a future
POSIX shim, any multi-process service). Two things needed deciding
up front: what happens to a message that's sent before anyone is
receiving it, and how big a message is allowed to be.

## Decision

**Synchronous rendezvous, not a buffered queue.** `Endpoint::send` does
not complete until a receiver is actually present to take the message —
this is the seL4/L4 model, not a mailbox or pipe. `Endpoint`'s internal
state (`ipc::endpoint::Slot`) is a single slot with three states —
`Empty`, `SenderWaiting`, `ReceiverWaiting` — never a growable queue.
The alternative (an unbounded or even a fixed-but-large per-endpoint
buffer) would let a process exhaust kernel memory purely by sending
messages faster than anyone reads them, with no rights check involved —
a resource-exhaustion channel that bypasses the capability model
entirely. A single-slot rendezvous makes that structurally impossible: a
second sender arriving while one is already waiting is a caller bug
(`try_send_inner` panics on it), not a queue that silently grows.

Both sides of a rendezvous can be either a kernel task (waiting as a
`Waker`, suspending itself by returning `Poll::Pending`) or a process
(waiting as a `Pid` — though this milestone's syscall path only ever
uses the non-blocking `try_send`/`try_recv_nonblocking` entry points, so
a process never actually suspends on IPC yet; see the "not yet
implemented" note below). `Endpoint` records which kind of waiter is
present in a `Waiter` enum and delivers to it uniformly, without needing
to know whether the other side is a task or a process — that distinction
is resolved by the caller (the executor for tasks, the syscall layer for
processes), not by `Endpoint` itself.

**Small, fully inline, register-passed messages.** A `Message` (defined
once in `tarnos-abi`, shared by kernel and userland so the two sides of
the syscall boundary cannot drift) is a `tag: u64` plus
`MESSAGE_INLINE_WORDS = 4` inline `u64` words — 40 bytes total, passed
entirely in registers on both `send` and `recv`. This milestone needs no
user-pointer validation for the common case: there is no pointer to a
user buffer to check bounds or ownership on, because the whole message
already lives in registers by the time it crosses the trap boundary.

`ipc::message::MessagePayload` is an enum with `Inline(Message)` as the
only variant actually constructed, and an `OutOfLine { page: PhysAddr,
len: usize }` variant that exists but is never built this milestone —
the designed extension point for larger payloads (e.g. a future POSIX
`write(2)` buffer, referenced by a mapped page rather than copied through
registers) without reshaping every call site that touches a message
today.

**Discovered, not anticipated, limitation:** the inline capacity is small
enough that it was hit during this milestone's own development — an
early `init` greeting string ("Hello from userspace, TarnOS is alive!",
38 bytes) exceeded 32 bytes of packable payload and `Message::from_str_lossy`
silently truncated it mid-word, producing garbled output. `from_str_lossy`
truncates by design (documented as such) rather than erroring, since
bounds-checked string transport belongs on top of `OutOfLine`, not as a
special case of the inline path. The fix was shortening the demo string,
not changing the transport — a reminder that "fits inline" is a real
constraint call sites must mind until `OutOfLine` exists.

## Consequences

- Every IPC round trip this milestone is provably bounded: no kernel
  buffer, no allocation on the send/receive path, no message loss modulo
  the caller's own logic (a duplicate `try_send` on an already-waiting
  slot is a panic, not silent data loss).
- A process-side `recv` with no sender waiting, or a process-side `send`
  with no receiver waiting, cannot actually block yet — `Endpoint::try_send`
  records the process as a waiting sender but nothing re-wakes it later,
  and there's no `SyscallError::WouldBlock`-driven suspend/resume path
  wired to the scheduler. This milestone's demo works around this by
  construction (the console server is always polled to its first
  `recv().await` before `init` is created, so a receiver is always
  already present) rather than by handling the general case. Making a
  process actually block on IPC — suspending it in the scheduler and
  resuming it when `Endpoint` later delivers — is unimplemented and is
  the next real IPC milestone.
- Any future POSIX/Linux syscall that needs to move more than 40 bytes
  (`read`, `write`, `mmap`-backed transfers) will need `OutOfLine` (or
  something like it) actually built out, plus a decision about how a
  receiver validates and maps a sender-supplied physical page — deferred
  as unstarted work, not designed in detail yet.
