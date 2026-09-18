# 0025: The Scheduler's Own State Is Guard-Paged; Two Clean Null Results Redirect the Search

## Status

Accepted. Permanent, zero-cost infrastructure added (kept regardless of
outcome — see Consequences), but neither experiment this round caught
the corruption in the act. Root cause remains open. Both results are
reported honestly because they meaningfully narrow the hypothesis
space, not because they found the bug.

## Context

ADR 0024 left the investigation with the cleanest evidence yet that the
long-running cross-core corruption is a genuine wild write landing on
scheduler-owned memory: a `RingBuffer::pop` bounds panic on `ready`'s
own `head` field, a value the ring buffer's own logic (already
proptest-verified) can never produce out of range on its own. Every
prior capture across this investigation (ADRs 0012–0024) had shown a
*symptom* consistent with this — a wrong `Pid`, a mismatched
generation, a corrupted core index — but never proof that the write
itself, not just the value later read, was the problem.

The standing methodology for this investigation has been to test one
falsifiable hypothesis per round rather than staring at code hoping to
spot the bug. This round's hypothesis: if something is wildly writing
into the scheduler's own state, relocating that state onto its own
dedicated pages with unmapped guard pages immediately adjacent should
turn the *next* such write into an immediate, attributable page fault
naming the exact instruction responsible — the same "slot + guard page"
shape this codebase already uses for kernel stacks
(`task::process::init_kernel_stacks`) and double-fault stacks
(`arch::x86_64::gdt`), applied here to `Inner`'s own fields instead of
a stack.

## Implementation

`Inner` (the `SCHEDULER`-locked struct holding `processes`,
`generations`, `ready`, and `current`) used to embed all four fields by
value, living wherever the linker placed the `static SCHEDULER:
SpinLock<Inner>` that owned it. Two variants were built and tested in
sequence:

**Phase 1a** relocated all four fields together as a single unit
(`GuardedState`) into one guard-paged region: one guard page below, N
pages of real data, one guard page above.

**Phase 1b** (built after 1a's null result — see below) went further:
each of the four fields now gets its *own* independently guard-paged
slot, spaced `SCHED_FIELD_STRIDE` (64 KiB — comfortably more than any
of these fields' own size) apart, all under a shared base
(`SCHED_STATE_BASE = 0xffff_9700_0000_0000`, sitting cleanly between
the existing idle-stack and kernel-stack regions). `Inner`'s fields are
now `&'static mut` references into these four separate regions rather
than a nested struct — `&mut [T; N]`/`&mut RingBuffer` auto-deref for
indexing and method calls exactly like the plain, by-value fields they
replace did, so no call site anywhere else in this ~2000-line module
changed syntactically except construction. The one genuine exception,
caught immediately by the compiler: `remove_from_ready_queue` used to
do `sched.ready = kept` (replacing the whole `RingBuffer` by value),
which now needs `*sched.ready = kept` — writing *through* the
guard-paged reference into the same still-guarded memory, rather than
silently rebinding the reference itself to point at `kept`'s ordinary,
unguarded stack storage instead.

Both `SCHEDULER` itself (now a `Once<SpinLock<Inner>>`, populated by a
new `task::scheduler::init()` called from `main.rs` right alongside
`task::process::init_kernel_stacks()` — the exact same ordering
requirement, for the exact same reason: every process's `AddressSpace`
takes its one-time kernel-half PML4 snapshot at creation time, so this
region must already be mapped before the first one is ever built) and
every one of the ~21 call sites that used to write `SCHEDULER.lock()`
directly were mechanically updated to go through new `scheduler_lock()`
/ `scheduler_force_unlock()` helpers instead. This was a bounded,
low-risk refactor: only the *outer* lock-acquisition syntax changed,
never the *inner* field-access logic.

## Result: two clean, informative null results

Both phases passed the full 22-scenario regression suite and a boot
smoke test before being stress-tested, confirming neither relocation
changes behavior on any correct-path scenario.

Each phase was then stress-tested against the established pure-yield
reproduction (`-smp 4`, 17s timeout, 40 runs):

- **Phase 1a**: 37/40 panics — none inside or adjacent to the new
  guarded region, none hitting any `switch_to` hardening assertion.
- **Phase 1b**: 31/40 panics — same result: nothing landed in or near
  any of the four newly separated guarded regions.

Both rates sit squarely within this investigation's usual batch-to-batch
noise (roughly 75–90% across every round so far), so neither
experiment perturbed the underlying corruption's behavior — it simply
never triggered either guard.

## What this rules out, and what it points to instead

Two independent, differently-grained experiments failing to catch
anything is itself a meaningful result, not merely "try again harder."
Guard pages can only ever catch one specific failure shape: a write
whose *address* lands outside the memory it was supposed to target.
They cannot catch a write that lands *inside* correctly-mapped,
correctly-owned memory but at the *wrong time* — i.e., a genuine
missing-synchronization bug, where two cores mutate the same
in-bounds memory without the `SCHEDULER` lock actually serializing
them. Phase 1b in particular was specifically designed to also catch
"wrote past the end of field A and landed on adjacent field B," and
still came up empty.

This shifts the most likely remaining explanation: not a wild pointer
computed from unrelated code landing on scheduler memory by chance
(both granularities of that hypothesis are now cleanly ruled out), but
a race — some path that reads or mutates `Inner`'s data (or the state
`RingBuffer::push`/`pop` themselves maintain) without correct mutual
exclusion. This is consistent with, and sharpens, ADR 0022's own
closing recommendation ("a fresh, adversarial line audit of
`switch_to`'s exact decision sequence for a *logic* bug ... in an order
that could produce a wrong answer for some interleaving") — the
target is now more specifically a lock-discipline gap than an
arbitrary logic error.

## Testing

- `cargo run -p xtask -- build`: clean, both phases.
- `cargo run -p xtask -- test-fault`: clean single-panic boot smoke
  test, both phases — confirms the guard-paged relocation itself
  doesn't break the boot sequence before committing to a full suite.
- Full 22-scenario regression suite: green, run independently for both
  phases — including the cross-core kill/spawn/wait/IPC scenarios that
  most heavily exercise every one of `Inner`'s fields.
- Pure-yield reproduction, `-smp 4`, 17s timeout: 40 runs each phase
  (37/40, 31/40) — both clean null results as described above.
- Every ISO build verified via its own boot-log line and
  `strings ... | grep -c "pure-yield process"` before trusting a batch
  against it.

## Consequences

- The guard-paged relocation (Phase 1b's per-field version; Phase 1a's
  code no longer exists, superseded rather than kept alongside it) is
  **permanent, kept regardless of this round's null result** — it adds
  real, zero-cost-on-the-happy-path defense-in-depth against any wild
  write into the scheduler's own state, including ones unrelated to
  this specific investigation, and the negative result itself is
  now-established, reusable evidence rather than a reason to revert.
  Any future occurrence of exactly this failure shape will now
  self-report immediately with an exact faulting instruction, in this
  or any future investigation.
- Two failure shapes for the corruption are now cleanly closed: a wild
  write overshooting `Inner`'s entire footprint from outside, and one
  landing on an adjacent field within it.
- The next productive step is a dedicated lock-discipline audit: every
  path that can read or mutate `sched.ready`/`sched.processes`/
  `sched.generations`/`sched.current` (or the raw `RingBuffer`
  methods specifically) for one that runs — even briefly, even only
  under a specific interleaving — without holding `SCHEDULER`'s lock.
  `percpu::PerCpuSlot.current` (the lock-free mirror `set_current`
  already maintains alongside `Inner.current`, by design, for
  cross-core polling without contending `SCHEDULER`) is the one
  place already known to intentionally read this state outside the
  lock, and is the natural place to start.
- `Process::trap_frame` (heap-allocated, separate from `Inner` — ADR
  0023's double-fault evidence pointed there) remains unguarded and is
  the natural target for a third guard-page experiment if the
  lock-discipline audit comes up empty too.
- `test-kitchen-sink` stays out of `test-all`/CI. Root cause of the
  underlying corruption remains open.
