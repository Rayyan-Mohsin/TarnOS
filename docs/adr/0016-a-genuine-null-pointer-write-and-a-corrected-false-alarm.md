# 0016: A Genuine Null-Pointer Write Caught Live, and a Corrected False Alarm

## Status

Accepted (partial — see Consequences). No source changes. A direct
continuation of ADR 0015's live-debugging session, using the same fixed
setup to capture one more organic panic — and a correction to this same
investigation's own reasoning about it, recorded here so the mistake
isn't repeated.

## Context

Continuing to attach post-mortem (per ADR 0015) to fresh, organically
occurring panics, one capture initially looked like the clearest possible
evidence yet: the `current[core]` value `dump_cores_for_panic` (ADR 0014)
reported for the faulting core named a *different* process-table index
than the kernel-stack slot the fault's own stack pointer was physically
using. That looked, at first, like direct proof of the exact hazard
`switch_to`'s own double-dispatch assertion (ADR 0014) was built to catch
by a different mechanism.

It wasn't. Re-tracing the exact code path it happened in found a
mundane, fully-documented explanation, recorded below so the same
false alarm doesn't cost a future session the same time.

## A corrected false alarm: `current[core]` legitimately outruns the stack switch

`scheduler::switch_to` updates `current[core]` (and `gdt::set_kernel_stack`/
`syscall::set_syscall_kernel_stack`, pointing at the *newly dispatched*
process's own stack) — but the CPU does not actually start *using* that
new stack until much later: `finish_switch`'s own doc comment is explicit
that everything between `switch_to` returning and the entry stub's own
final `mov rsp, rax; iretq` still runs on the *outgoing* process's stack.
`context_switch::ring3_timer_tick` calls `on_timer_tick` (which may
dispatch a brand new process via exactly this path) *before* calling
`interrupts::send_timer_eoi()` — so if that later EOI call itself faults,
`current[core]` already names the newly-dispatched process while the
CPU is still physically executing on the *previous* process's own kernel
stack. A panic caught in that specific window will always show this
"mismatch," and it is by design, not a bug. Worth stating plainly for
whoever next reaches for `dump_cores_for_panic`'s output to attribute a
kernel-stack address to a process: cross-reference against exactly which
function faulted first (a fault inside anything called *before* the
scheduler dispatch in a given entry stub is trustworthy this way; a fault
inside anything called *after* it, like the tail EOI call here, is not).

## A genuine, unexplained finding

The panic this false alarm came from was itself real and still
unexplained: `page fault accessing 0x0 (error CAUSED_BY_WRITE) at
0xffffffff80042b31 -- core 0 was running raw pid 0x100000009`, with `rip`
resolving (via the loaded symbol file) to inlined code shared between
`tarnos_kernel::sync::SpinLock<pic8259::ChainedPics>::lock` and
`spin::Mutex::lock`'s own internals — i.e., a fault *while taking the
legacy PIC's lock*, called only from the PIT timer interrupt's
bookkeeping/EOI path (`arch::x86_64::interrupts`), itself only ever
driven on the BSP. The decoded `FaultFrameWithCode` showed `rdi = 0` at
the moment of the fault; the disassembly immediately around the faulting
instruction is a `mov %rsi, 0x50(%rsp)`-shaped store, consistent with the
CR2 address (exactly `0x0`) coming from a stack-relative computation that
should never legitimately reach address zero. This was not resolved
further this session — the exact source of the zero (a corrupted `rsp`,
a corrupted intermediate value that feeds the store's address, or
something else) needs a slower, single-step capture (`stepi` through the
handful of instructions right before the fault, watching each register)
rather than a single post-mortem snapshot, to pin down with confidence.

## What remains open

The root cause is still not found. This round's genuine contribution is
methodological: the `current[core]`-vs-stack-slot cross-check is now
known to have a real, easily-triggered false-positive window, and any
future use of it must first confirm the fault happened *before* a given
entry stub's own scheduler-dispatch call, not after. The `SpinLock<
ChainedPics>::lock` null-address write is a new, concrete, and still
open lead — notably the *first* capture this whole investigation (ADR
0012 onward) has tied to the legacy PIC/PIT path specifically, rather
than the LAPIC timer, IPC, or process-lifecycle paths every previous
finding implicated.

## Testing

No source changes this round; nothing to regress.

## Consequences

- Prevents a future session from re-deriving, and re-trusting, the same
  `current[core]`-vs-stack-slot false alarm this one initially did.
- A new, narrower, and previously-unseen lead (the legacy PIC lock path)
  is recorded for continuation, alongside the honest acknowledgment that
  it needs a single-stepped capture, not a post-mortem one, to resolve.
- Root cause remains open. `test-kitchen-sink` stays out of
  `test-all`/CI.
