# 0018: An Address-Space Use-After-Free — Root Cause Found, Partially Fixed

## Status

Accepted (partial — see Consequences). Implemented:
`kernel/src/task/scheduler.rs` (deferred `AddressSpace` drop for self-exit;
CR3-before-unlock ordering fix for the idle path),
`kernel/src/arch/x86_64/idt.rs` (rsp attribution on ring0 faults),
`xtask/src/main.rs` (`--features` actually wired up). `test-kitchen-sink`'s
own failure rate dropped from an established ~50% (`-smp 4`) to roughly
10-12%, a genuine ~4-5x improvement, but the corruption is not eliminated.

## Context

ADR 0017 left the investigation at: a genuine cross-core scheduling/IPI
protocol logic error, narrowed to the shared dispatch/wake path
(`on_timer_tick`/`switch_to`/`spawn`/`wait_for_child`/
`wake_blocked_process_locked`), with no specific line identified. This
round found and partially fixed the actual root cause, but also ran into
(and had to back out of) a real tooling bug along the way, worth recording
alongside the fix itself.

## A tooling bug invalidated a round of experiments

`cargo run -p xtask -- build --features kitchen-sink-test` looked like it
worked — no error, `xtask: build OK` — but `main`'s `"build"` arm never
parsed `rest` (the command's own trailing arguments) at all, and `build()`
itself never touches `build/tarnos.iso` in the first place (`iso()` does
that; `build` only compiles the kernel/userland ELFs). Several manual
QEMU stress batches this round were run believing they exercised a
freshly-rebuilt `kitchen-sink-test` image, while actually reading whatever
ISO `test-all`'s own last scenario (`test-smp-forced-preempt`) had most
recently assembled — including one batch that, in hindsight, was scoring
a completely different, much lower-workload scenario's own (much rarer)
failure mode. That data is discarded; `xtask/src/main.rs` now parses a
real `--features a,b,c` flag, shared by `build`/`iso`/`run`, and every
experiment this ADR reports was re-run and verified against a build
confirmed (via its own distinct boot log line) to actually be
`kitchen-sink-test`.

## The actual root cause: freeing a still-active address space

`switch_to`'s own doc comment already listed everything already
double-checked before use: the double-dispatch assert, the generation
check, the `cs`/`rip` canonical-address validation on the frame about to
be resumed. All of that protects the process-table's own bookkeeping and
the frame being resumed — none of it protects the *address space itself*
from being torn down while a core's own CR3 still points at it.

`terminate_current_process` (self-exit and fault-kill's shared path)
called `terminate_slot(pid, status)`, which finalized the process *and
immediately dropped its `Box<Process>`* — freeing its `AddressSpace`,
including its own PML4 frame, back to the physical frame allocator —
*before* `switch_to_next_or_halt` (called right after) ever switches this
core's own CR3 to anything else. For the entire window between that
`drop` and the eventual `switch_to`/`activate_idle_address_space()` call,
this exact core keeps executing kernel code — fetching every subsequent
instruction — through page tables rooted at a PML4 frame the allocator
now considers free. If any other core allocates that exact frame in the
meantime (a new process's own PML4, a new page table, anything), it
overwrites the very page tables this core is still walking on every
instruction fetch.

This was caught directly, for the first time, via a genuine capture
(enabled by a new per-core dispatch trace buffer added this round — see
below): a core's own tracked `current` correctly read `None` (idle), yet
its live `rsp` (from the fault frame) still pointed *inside a different
process's kernel-stack slot*, near its own top — consistent with a stale,
leftover return address on that recycled stack being popped and jumped
to. A second capture was even more direct: the CPU faulted trying to
*execute* the first instruction of a real kernel function
(`arch::x86_64::smp::idle_stack_top_addr`, identified from the exact
faulting address via `nm` on the kernel ELF) — a plainly legitimate,
always-mapped kernel `.text` address — with a "not present" page fault,
meaning the currently-active page tables (rooted at the dying process's
now-freed PML4) no longer described it correctly.

### The fix (self-exit path)

`terminate_slot` no longer drops what it finalizes — it returns
`Vec<Box<Process>>` instead. `terminate_current_process` holds onto that
`Vec` and threads it through `switch_to_next_or_halt` → `finish_switch` /
`abandon_process_stack_and_idle` → `idle_loop_trampoline`, dropping it
only *after* this core's own CR3 has actually moved to something else
(right after `switch_to`'s `process.address_space.activate()` in the
"found something ready" branch, or right after
`activate_idle_address_space()` in the "must idle" branch). Crossing the
raw stack switch in `abandon_process_stack_and_idle` needs the `Vec`
boxed into a single pointer-sized register argument (a `Vec` is three
words, and the existing raw `asm!` call already carries the halt message
and outgoing stack index the same way) — reconstructed and dropped on the
far side by `idle_loop_trampoline`.

### A second, related bug found while implementing the first

`idle_loop_trampoline` released `SCHEDULER`'s lock (`force_unlock()`)
*before* calling `activate_idle_address_space()` — reasonable-looking,
since neither one obviously depends on the other. But releasing that lock
is exactly the signal a cross-core `SYS_KILL`'s `terminate_process` waits
on: its own spin-wait exits the instant the *lock-free* `PerCpuSlot.current`
mirror clears (which happens before this trampoline even starts running),
then it immediately tries to re-acquire `SCHEDULER` for its own re-scan.
With the old ordering, that re-acquisition could succeed *before* this
core's own CR3 had actually moved off the evicted process's address
space — the identical use-after-free, just reached through eviction
instead of self-exit. Swapping the two calls (`activate_idle_address_space()`
now runs before `force_unlock()`) closes it the same way `switch_to`'s own
already-correct ordering does for the "found something ready" branch: a
killer's lock acquisition can't even begin until the CR3 switch is
already done, since the lock is a real spinlock and hasn't been released
yet.

### What's still open

`terminate_process`'s own "not found running anywhere" finalize branch —
reached once a target has genuinely stopped being `current` on every core
— still frees the target's `AddressSpace` immediately, on the *killer's*
core, without any deferred-drop protection of its own. Reasoned to be
safe (the previous owning core's own `switch_to`/`activate_idle_address_space`
call happens under the same `SCHEDULER` acquisition that clears
`current`, so by the time it reads `Empty` under a fresh lock acquisition
the CR3 switch must already be complete) but not proven by a targeted
adversarial test the way the two fixes above were. Given the measured
residual failure rate after both fixes, either this reasoning has a gap
not yet found, or an unrelated third mechanism is also contributing — see
Testing below.

## New diagnostics added this round

- **Per-core dispatch trace** (`scheduler.rs`): a small, always-recording
  ring buffer per core of every `set_current` transition (pid + a
  `RDTSC`-based ordering key), dumped by `dump_cores_for_panic`. Its own
  most important finding was about *itself*: an earlier version used one
  shared, contended `AtomicU64::fetch_add` for ordering, which alone
  suppressed `test-kitchen-sink`'s failure rate from ~50% to 0/105 runs —
  not a fix, just proof of how narrow this race already was. Switching to
  a per-core `RDTSC` read (no cross-core memory traffic at all) still
  didn't restore the baseline rate, which is what pointed at `SCHEDULER`'s
  own critical-section *duration* (not contention specifically) as the
  sensitive variable, not at the trace buffer's mechanism. This means the
  buffer is a real, permanent diagnostic for whatever it *can* still
  observe, but cannot be trusted to catch every occurrence of a race this
  sensitive to added latency on a `SCHEDULER`-locked path — and neither
  can any future fix attempt that happens to add work there. Any claim
  that a change "fixed" this bug needs verification across the full
  `-smp`/TCG-threading matrix from ADR 0017, not just a lower observed
  rate at one configuration.
- **`rsp` attribution on ring0 faults** (`idt.rs`): `page_fault_ring0`/
  `general_protection_fault_ring0` now report which kernel-stack slot (if
  any) both the faulting address/rip *and* `rsp` fall in, via a shared
  `describe_stack_addr` helper — previously only the faulting address/rip
  was attributed. This is what made the first genuine capture legible:
  distinguishing "a bad value got fetched as code" from "this core is
  genuinely executing on someone else's live stack" needs both.
- Also fixed a pre-existing mislabeling bug in that same message text:
  `describe_kernel_stack_address`'s `offset` is measured from a slot's own
  *base* (just above its guard page), not its top as the panic text had
  claimed since ADR 0013 — confirmed by cross-checking a real capture's
  reported offset against `KERNEL_STACK_SLOT_STRIDE`'s own arithmetic.

## Testing

- Full 22-scenario regression suite green after each of the three
  `scheduler.rs`/`idt.rs` commits this round.
- `test-kitchen-sink` (`-smp 4`, default MTTCG), 40-80 runs per
  configuration, using a build verified correct after the `xtask` fix:
  - Before either fix: established ~50% (ADR 0012/0017's own baseline).
  - After the self-exit deferred-drop fix alone: 6/60 panicked (10%).
  - After both fixes (self-exit + idle-path CR3/unlock ordering): 10/80
    panicked (12.5%) — within noise of the previous measurement, not a
    further measurable improvement in this sample, though the second fix
    closes a real, independently-reasoned hazard and is kept regardless.
  - Remaining captures still show the same signature (a live `rsp` inside
    a kernel-stack slot the core's own `current` doesn't name, or an
    instruction fetch/LAPIC MMIO write through a plainly corrupted
    pointer), consistent with the *same class* of bug persisting via the
    still-open `terminate_process` path above, not a new, unrelated one.

## Consequences

- The root cause of ADR 0012 through 0017's entire investigation is now
  understood in concrete, mechanistic terms: a process's `AddressSpace`
  (specifically its PML4 frame) could be freed and reallocated while a
  core's own CR3 still pointed at it, for as long as that core kept
  executing kernel code before its own next context switch actually
  landed. Every future change to process termination, eviction, or
  address-space teardown must preserve the invariant this ADR's fixes
  establish: never drop a `Box<Process>` (or otherwise free an
  `AddressSpace`) until the specific core that was using its CR3 has
  provably switched to a different one, under the same lock that
  serializes against every other transition.
- `test-kitchen-sink` stays out of `test-all`/CI — a 4-5x improvement is
  not reliability.
- The next session's most direct path forward is closing the
  `terminate_process` "not found anywhere" branch the same way: prove
  (with a targeted adversarial test, not just re-running the full
  scenario) whether a killer can ever observe a target as gone from every
  core before that core's own CR3 switch is complete, and if so, apply
  the same deferred-drop treatment there.
