# 0013: Peer-Death Wake for Blocked IPC, and Live Forensics on the Remaining Corruption

## Status

Accepted (partial — see Consequences). Implemented across
`kernel/src/task/scheduler.rs`, `kernel/src/ipc/endpoint.rs`,
`libs/tarnos-abi/src/lib.rs`, `libs/tarnos-kcore/src/captable.rs`.

## Context

ADR 0012 closed two real orphaned-lock deadlocks on panic but left
`test-kitchen-sink`'s underlying corruption un-root-caused, at roughly a
30% pass rate. Before continuing that hunt, the first question worth
answering directly was whether this environment's own limits — no
`/dev/kvm` (nested virtualization not passed through), so QEMU runs
under software TCG emulation only, with 4 emulated cores sharing 4 real
host cores — were producing *false* failures (the guest genuinely
needing more wall-clock time than a slow host could give it) rather than
exposing a real bug.

That was directly testable: `test-kitchen-sink`'s harness (`run_scenario`
in `xtask`) always sleeps its full fixed timeout before killing QEMU and
reading the log, regardless of whether the test already finished — so
tripling the timeout (20s → 60s) costs nothing to try. It made no
measurable difference: every "IPC round trip never reported a result"
hang still hung for the full 60 seconds with zero further output, and
panics still occurred at the same rate, just with more idle time
afterward. A genuine hang or an already-fired panic does not resolve
with more wall-clock time; only a test that's still doing legitimate
work would. This ruled out "just needs more time on this host" as an
explanation and redirected the investigation back to the kernel itself.

## Decision

### 1. Wake a blocked `SYS_RECV` when its last live sender dies (`PeerClosed`)

Redirected by the timeout experiment, the next step was characterizing
*which* failure mode was actually dominant. It was "IPC round trip via
echo-child never reported a result" — and tracing that mechanism found a
concrete, previously-undocumented IPC design gap, independent of
whatever corruption causes a process to fault in the first place: when a
process dies (self-exit, or a fault-kill via the ordinary ring-3
fault-isolation path from ADR 0005), nothing ever notified a *different*
process still blocked in `SYS_RECV` waiting for a message only the dead
process could ever have sent. That receiver blocked forever, regardless
of why its peer died — exactly matching `test-kitchen-sink`'s own IPC
orchestrator hanging after `echo-child` faulted before ever replying.

`wake_orphaned_receivers` (`task/scheduler.rs`), called from
`terminate_slot`/`terminate_process` right after they release
`SCHEDULER` (mirroring `pending_drops`'s own established "finish the
locked bookkeeping first, do the rest after" shape): for each of a dying
process's own `SEND`-rights capabilities, checks whether any *other*
currently-occupied process still holds a `SEND`-rights reference to the
exact same `Arc<Endpoint>` (`Arc::ptr_eq`). This needs no new
bookkeeping to stay correct — this kernel has no way to *revoke* a
capability once granted (only a whole process dying ever removes one,
via `SYS_GRANT`'s one-shot parent-to-suspended-child transfer), so the
live-sender set can only ever shrink, never grow back in the gap between
checking it and acting on it. If none remain, wakes that endpoint's
blocked process-receiver — never a `Waiter::Task`, a kernel task's own
unrelated wait (e.g. `console_server`'s) is left alone — with a new
`SyscallError::PeerClosed`.

Two passes, deliberately never nesting `SCHEDULER` with `ipc::Endpoint`'s
own lock: pass one (SCHEDULER locked) decides which endpoints are
orphaned; pass two (per-endpoint, `Endpoint`'s own lock only) takes the
waiting receiver and wakes it. This codebase has never needed a
lock-ordering rule between the two, and this change doesn't introduce
one.

Deliberately receive-side only, not send-side-symmetric: the reverse (a
blocked `SYS_SEND`'s only possible *receiver* dies first) would need
selectively draining `Waiter::Process` entries out of the middle of
`ipc::endpoint::Slot`'s `SendersWaiting` ring buffer while preserving any
interleaved `Waiter::Task` ones and delivery order — real, but
unobserved in this codebase's own current usage, where every caller's
receiver is already alive and waiting before the sender ever calls
`SYS_SEND`. Deferred rather than guessed at.

**Effect, measured via a 40-run stress batch:** `test-kitchen-sink`'s
pass rate rose from ~30% to 45%, and "IPC round trip never reported a
result" — previously the single dominant failure — dropped to 3/40
(7.5%). The new leading failure mode became direct kernel panics
(14/40), which this fix does nothing to address — it closes a real,
general IPC gap, not the underlying memory corruption.

### 2. Live forensics on a remaining panic

With the IPC-hang noise reduced, a fresh panic was caught live (the same
GDB-attached-to-a-running-QEMU-instance technique ADR 0012 introduced)
to see whether the corruption's signature had changed. It hadn't in kind,
but this specific catch gave the clearest evidence yet:

```
[KERNEL PANIC] kernel/src/arch/x86_64/idt.rs:153:5: page fault accessing
0xffff98000002cac8 (error PageFaultErrorCode(PROTECTION_VIOLATION |
INSTRUCTION_FETCH)) at 0xffff98000002cac8
```

The faulting address is a **kernel-stack address** (in the
`0xffff9800_00000000`-based range `task::process::KERNEL_STACKS_BASE`
uses), and the error is an instruction fetch rejected by the no-execute
bit every kernel stack page carries. Reading the raw `FaultFrameWithCode`
directly out of guest memory via the QEMU monitor's own `x` command
(GDB's `x` command itself was unusable against this bare-metal remote
target — every attempt failed with "operation not available on integers
of more than 8 bytes" regardless of address or size, worth noting for
whoever continues this) confirmed the mechanism precisely: the stack
slot at `rsp - 8` relative to the fault (address `...ca80`) held the
value `0xffff98000002cac8` — the *exact* faulting RIP. The frame's own
saved `rsp` was `...ca88`, exactly 8 bytes above that slot. This is the
signature of a `ret` instruction popping a corrupted 64-bit value —
which should have been a valid kernel `.text` return address — off the
stack and jumping to it, landing back inside stack memory itself rather
than code.

Two concrete hypotheses this raised were checked directly against the
code and ruled out, not just retested empirically:

- **A slot-stride overlap letting two adjacent kernel-stack slots share
  physical pages.** Traced `kernel_stack_slot_base`/`kernel_stack_top`/
  `KERNEL_STACK_SLOT_STRIDE`'s arithmetic by hand: slot `N`'s
  `kernel_stack_top()` lands exactly on slot `N+1`'s own base address,
  which is that next slot's *guard page* (deliberately left unmapped by
  `init_kernel_stacks`'s `for i in 1..=KERNEL_STACK_PAGES` loop, which
  skips relative offset 0). No overlap: a stack pointer corrupted high
  enough to leave slot `N`'s own writable region lands in unmapped
  guard-page memory and page-faults immediately (not-present, not
  instruction-fetch) — a different, more immediately obvious failure
  than the one actually observed, so this isn't it.
- **Plain kernel-stack overflow from deep call chains under load.**
  Already tested in an earlier session (`KERNEL_STACK_PAGES` doubled
  from 4 to 8, i.e. 16 KiB → 32 KiB) with no change in failure rate,
  before being reverted back to 4 — ruling this out again here would
  have just repeated that experiment.
- **`test-kitchen-sink`'s own new "`KS_HEAP_OK` never reported" failure
  (a fresh signature that appeared post-fix-#1, absent before) being a
  distinct, heap-specific bug.** Traced `ks_heap_orchestrator`'s own
  protocol: it uses `SYS_WAIT`, not `SYS_RECV` — and `SYS_WAIT`'s
  wake-on-child-termination path (`take_and_finalize_slot`'s handling of
  `wait_waiter`) already unconditionally resolves on *any* child
  termination, fault-kill included, entirely independent of this ADR's
  `PeerClosed` fix. A hung `SYS_WAIT` here means `ks_heap_orchestrator`
  itself never got the chance to run its own report at all — i.e. this
  is the *same* general corruption striking a different process, not a
  new, separate bug to chase.

## What remains open

The corrupted-return-address mechanism is now precisely characterized —
some code path pops a stack-resident value expecting a valid
kernel-`.text` return address and gets a stray kernel-stack address
instead — but the *write* that put a wrong value there was not caught in
the act; only its downstream effect (the eventual `ret`) was observed.
Continuing this needs either a systematic reduction of `test-kitchen-sink`
to the smallest concurrent-workload combination that still reproduces it
(to narrow which specific interaction is responsible, the way ADR 0010's
own races were eventually isolated), or a more invasive tracing approach
this session didn't reach — e.g. poisoning every kernel stack with a
recognizable sentinel pattern at `init_kernel_stacks` time and checking,
on each context switch, whether anything outside the currently-owning
process's own in-use range has been disturbed.

## Testing

- Timeout experiment: 15-run batch at 60s (vs. the normal 20s), compared
  directly against the existing 20s baseline — no rescued hangs, ruling
  out "just needs more time."
- 40-run stress batch of `test-kitchen-sink` with the `PeerClosed` fix,
  compared against the pre-fix baseline: pass rate 30% → 45%, dominant
  failure mode shifted from IPC hangs to direct panics.
- IPC-focused regression: `test-spawn-ipc`, `test-blocking-ipc`,
  `test-double-send`, `test-smp-send-cross-core`, `test-smp-wait-cross-core`
  all still pass.
- Full ~23-scenario regression suite (`xtask test-all`), green.
- `cargo test -p tarnos-kcore -p tarnos-abi` (45 passing) and
  `cargo clippy -p tarnos-kcore -p tarnos-abi --all-targets -- -D warnings`,
  both clean.
- `test-kitchen-sink` still deliberately not wired into `test-all`/CI —
  45% is real progress, not reliability.

## Consequences

- A real, general IPC correctness gap is closed: a blocked `SYS_RECV`
  can no longer wait forever on a peer that will never come back,
  regardless of what killed it. This matters beyond `test-kitchen-sink`
  — any future TarnOS program pair with the same shape (a receiver
  waiting on a sender that might fault) is now safe by construction.
- `test-kitchen-sink`'s pass rate improved measurably (30% → 45%) but
  the milestone's original goal — reliable, CI-wired — is still not met.
  The remaining corruption is now precisely characterized (a corrupted
  return address landing back in kernel-stack memory) but not yet
  traced to its write site.
- The QEMU-monitor-based memory read (`x/Ngx <addr>` over the monitor
  socket) is now a proven fallback for this project's live-debugging
  technique, for the cases where GDB's own `x` command doesn't work
  against this bare-metal remote target.
- `test-kitchen-sink` stays out of `test-all`/CI.
