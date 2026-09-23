# 0005: Fault Isolation and Blocking IPC

## Status

Accepted. Implemented across `kernel/src/arch/x86_64/{idt,context_switch}.rs`,
`kernel/src/task/scheduler.rs`, `kernel/src/ipc/endpoint.rs`, and
`libs/tarnos-kcore/src/endpoint.rs`.

## Context

Milestone 1 proved the architecture — capabilities, IPC, async
execution, and process isolation all genuinely worked together. But two
of its simplifications were load-bearing in a way that undermined the
very isolation the milestone existed to demonstrate:

- **Any CPU exception, anywhere, panicked the whole kernel.** A process
  dereferencing a bad pointer took the entire machine down with it —
  functionally no different from a monolithic kernel with no process
  boundaries at all.
- **IPC never actually blocked.** `sys_send`/`sys_recv` either completed
  immediately or returned an error; a process that genuinely needed to
  wait had no way to. The milestone's own demo only worked because boot
  code carefully guaranteed a receiver was always already primed —
  a scaffold, not a real capability. Worse, calling `sys_send` twice
  before anyone received the first message hit an already-occupied
  single-slot rendezvous and panicked — any process holding an ordinary
  `SEND` capability could crash the kernel this way.

Milestone 2 closed both gaps, in service of a single goal: prove the
isolation and blocking guarantees the architecture claims are real,
under conditions a hostile or merely buggy process can actually trigger
— not just conditions a carefully-choreographed demo avoids.

## Decision

### Fault isolation is decided by privilege level, not by vector

Every exception handler in `idt.rs` now asks one question before doing
anything else: did this fault happen at CPL 0 (the kernel's own code)
or CPL 3 (a process)? The answer is read directly off the saved CS
selector — the same technique `context_switch`'s timer entry stub
already used to tell a preempted process apart from preempted kernel
code. CPL 0 is unconditionally fatal: the kernel's own invariants broke,
and there is no principled way to "recover" from that by killing some
process, because the fault may not even be attributable to one. CPL 3
terminates exactly that process and resumes the scheduler — reusing the
same drop-and-reschedule path (`task::scheduler::terminate_current_process`)
`SYS_EXIT` already used, since a fault-triggered kill and a voluntary
exit are, from the scheduler's point of view, the same event.

Only four vectors get this treatment (divide error, invalid opcode,
general-protection fault, page fault) — the ones a process's own
malformed code or data can plausibly trigger. Double fault stays
unconditionally fatal regardless of CPL: it means the trap-handling
machinery itself is already in a broken state, which is not something
safe to build a "kill the process, keep going" response on top of.

Making the CPL-3 path actually redirect execution to a *different*
process's saved state — not just return to wherever the fault
occurred — needed the same hand-rolled, full-register-capture entry
stub the timer already used, for the same reason: the compiler-generated
`extern "x86-interrupt"` ABI used for every other handler in this file
gives no way to control which registers get restored on the way out,
because every other handler only ever returns to where it was called
from.

### Kernel stacks must live in the shared kernel half, not per-process

A direct consequence of processes now genuinely switching between each
other (rather than a lone process alternating with itself every timer
tick, milestone 1's only tested case): the code that performs a switch
keeps running *on the outgoing process's own kernel stack* for a while
after `AddressSpace::activate()` changes CR3. If that stack exists only
in the outgoing process's own page tables — the milestone's first
attempt at guard-paged per-process stacks — the very next stack access
after the switch faults, because the address the CPU's RSP already
points at just vanished from the newly-active page tables. Every
process's kernel stack (still individually guard-paged, for the reason
task 20 introduced them: making an overflow fault instead of silently
corrupting adjacent memory) is instead mapped into the shared kernel
half, once, for every possible process slot, before any process's own
address space is ever created — exactly how the kernel heap and the
double-fault stack were already handled. The lesson generalizes: any
memory a switch's *own code* touches while it's running must be visible
from every address space a switch could land in, not just the one
being switched away from.

### IPC blocking is resolved differently for a process than for a task, and a `Slot` has to know it

A blocked `Task` can only ever retrieve a value by being woken and
re-polling — a `Waker` carries no payload. A blocked `Process` has no
poll loop to do that with; the syscall path suspends it by saving its
trap frame and switching away, and it must be *resumed with its answer
already sitting in its saved registers*, because there is no code of
its own left running to go fetch that answer. `ipc::Endpoint` resolves
this asymmetry: delivering to a queued `Task` receiver re-queues the
message where that receiver's own next `try_recv` will find it (mirroring
how the rendezvous already worked before this milestone), while
delivering to a queued `Process` receiver calls back into `try_recv`
*synchronously, on that process's behalf*, then writes the result
directly into its saved trap frame via `task::scheduler::wake_blocked_process`
before pushing it back onto the ready queue.

That asymmetry produced a real bug worth recording, because the fix is
itself a hard-to-reverse property of the design: when a message is
re-queued for a `Task` receiver's later collection, the *sender*
bundled alongside it must not be the original sender's real identity —
that sender was already told `Delivered` synchronously, the moment the
message was accepted, and has moved on. Re-notifying it when the
message is *actually* collected later would tell an already-resolved
process it just completed a send it made once, correctly, some time
ago — silently corrupting scheduler state (observed as a process
getting marked `Ready` and re-queued a second time, spuriously).
`tarnos_kcore::endpoint::Waiter::None` — a "nobody: notifying this is a
no-op" placeholder — exists specifically to make this class of bug
impossible to reintroduce: the type system now requires every code path
that displaces a waiter to say explicitly whether there is a real
sender to tell.

### The sender side of a rendezvous is a bounded queue; the receiver side stays a single value

Fixing the double-send panic required admitting that two different
processes (or one process, retried) both wanting to send to the same
endpoint before anyone receives is an ordinary scenario, not a caller
bug — so the sender side of `Slot` became a FIFO queue, bounded to
`task::scheduler::MAX_PROCESSES` (the most that could ever simultaneously
exist to be queued). The receiver side deliberately did *not* become a
queue: with more than one `Task` receiver queued, a second one could
race the first to re-poll and steal a message meant for it, since
nothing distinguishes which queued receiver a re-queued message was
"for." A `Process` receiver doesn't strictly need this restriction (it's
resolved synchronously, no re-poll race possible) — but the two waiter
kinds share one `Slot`, so the stricter case governs, and a second
receiver reports a bounded-resource error instead.

### A new syscall error replaces `WouldBlock`

`SyscallError::WouldBlock` existed to name "this would need to block,
which isn't supported yet" — a placeholder for exactly the gap this
milestone closed. With blocking now real, nothing returns it anymore;
it was replaced with `ResourceExhausted`, covering the one bounded
failure mode blocking IPC can still hit: an endpoint's sender queue (or
the scheduler's process table, via the same error, once a spawn syscall
exists) already fully committed. The encoding convention — a negative
`i64` in RAX, matching Linux's sign-check ABI — is unchanged; see
`docs/adr/0004-posix-abi-seam.md`.

## Consequences

- A process can now fault, misbehave, or block without threatening the
  rest of the system — the isolation and blocking guarantees are
  exercised by dedicated integration tests
  (`xtask test-fault-isolation`, `test-blocking-ipc`, `test-double-send`)
  under conditions a process actually controls, not just the
  original demo's cooperative ordering.
- The "kill the process, not the kernel" policy only covers four
  exception vectors. A future milestone that wants the same treatment
  for others (alignment checks, SIMD exceptions) can follow the exact
  same CPL-based pattern — it is not vector-specific in principle, only
  in what was actually implemented.
- Any future addition of new shared, boot-time-mapped kernel memory
  (per the kernel-stack lesson above) must be mapped before the first
  `AddressSpace::new()` call, or reached only through the master mapper
  after that point — never lazily through a single process's own
  address space if anything besides that one process might ever
  observe it during a switch.
- `Waiter::None` is a narrow, specific fix, not a general "maybe nobody"
  escape hatch — it exists to express one precise fact (a displaced
  sender that was already resolved) and should not be reached for by
  future code as a generic placeholder.
- Multiple queued receivers on one endpoint remain unsupported (reported
  as a bounded-resource error, not implemented as a queue) — a real,
  if narrower, limitation than the sender side's, and the one piece of
  the original double-panic bug class this milestone deliberately left
  as a reported error rather than a queue, for the reasons in the
  Decision section above.
