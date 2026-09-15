# 0017: The Corruption Requires Multiple Cores But Not Hardware Parallelism

## Status

Accepted (partial — see Consequences). Implemented: `kernel/src/arch/x86_64/idt.rs`
(double-fault diagnostics only). The rest of this ADR reports controlled
experiments and their conclusions; no root-cause fix yet.

## Context

Every investigation from ADR 0012 through 0016 treated `test-kitchen-sink`'s
corruption as a cross-core memory race — two cores' hardware-concurrent
access to the same memory tearing a read or write. That framing was never
directly tested. Prompted by a direct challenge that a 50% failure rate is
not an acceptable place to stop investigating, this round ran the
experiments that should have come first: does the corruption actually need
real hardware parallelism, and does it need multiple cores at all?

## The experiment

All four configurations use the exact same kitchen-sink-test kernel image,
varying only QEMU's own core count and TCG threading mode:

| Configuration                          | Runs | Pass | Fail rate |
|-----------------------------------------|------|------|-----------|
| `-smp 1`                                 | 20   | 20   | **0%**    |
| `-smp 2`                                 | 100  | 99   | **~1%**   |
| `-smp 4` (default, multi-threaded TCG)   | 20   | 10   | **50%**   |
| `-smp 4`, `-accel tcg,thread=single`     | 20   | 5    | **75%**   |

Two conclusions follow directly and are not compatible with the "hardware
race" framing:

- **The corruption cannot happen with only one core.** `-smp 1` still runs
  every other piece of machinery this investigation has scrutinized — the
  per-core LAPIC timer, forced preemption, `SYS_SPAWN`/`SYS_WAIT`/`SYS_EXIT`,
  IPC, heap growth — with zero failures across 20 runs. Whatever the bug is,
  it structurally requires a *second* core to exist, not just heavy
  concurrent activity.
- **Serializing every core onto one host thread does not fix it — if
  anything, it's worse.** `-accel tcg,thread=single` forces QEMU to
  interleave all four vCPUs' instruction streams on a single host thread,
  eliminating any possibility of genuinely simultaneous memory access
  between them. If the bug were a torn read/write requiring true hardware
  concurrency, this should have reduced or eliminated it. It didn't — the
  failure rate went *up* (50% → 75%), consistent with the change in
  interleaving timing shifting how often a bad *sequence* of events occurs,
  not with removing a hardware race.

Together, this reframes the entire bug: it is a **logic error in the
cross-core protocol itself** (scheduling dispatch, IPI-based eviction, or
cross-core wake) — something reachable through *any* interleaving of
multiple distinct cores' state machines, not one requiring true
simultaneous memory access. Every previous ADR's search for a missing lock,
a missing memory barrier, or a torn write was aimed at the wrong class of
bug.

## Narrowing which workload is responsible

`test-kitchen-sink` runs four orchestrators concurrently (IPC round-trip,
heap growth, spawn/wait lifecycle, cross-core kill) plus two background
`SYS_YIELD`-pressure processes. Of these, only the kill orchestrator's own
mechanism ([`task::scheduler::terminate_process`]) is structurally
cross-core-only — at `-smp 1` its "is the target running on a different
core" check can never find one, so it always takes the trivial
same-core-finalize path, never exercising the IPI/eviction protocol at all.
That made it the first suspect.

Three temporary, uncommitted experiments (spawning `test-kitchen-sink`'s
process set with one workload's spawn code deleted, then reverted) at
`-smp 4`, 40 runs each, counting genuine `[KERNEL PANIC]` occurrences only:

- **Kill orchestrator removed:** 10/40 panicked (25%).
- **Heap orchestrator removed:** 7/40 panicked (17.5%).
- **Both background `SYS_YIELD`-pressure processes removed** (IPC, heap,
  lifecycle, and kill orchestrators all still present): 6/40 panicked
  (15%) — pressure alone contributes about as much as heap does, and
  removing it still leaves a substantial rate.

No single removal comes close to eliminating the panics, and no removal's
effect stands out as dominant — kill, heap, and pressure each account for
roughly the same order-of-magnitude share (15–25%) on their own. This
argues against a defect isolated to any one workload's own syscall logic,
and *for* a shared mechanism every workload exercises identically: the
scheduler's own per-core dispatch loop (`scheduler::on_timer_tick` →
`switch_to`, driven by every core's independent LAPIC timer against the
one shared ready queue). That mechanism is the one thing structurally
common to every remaining configuration in this table — IPC and lifecycle
alone (kill, heap, *and* pressure all removed) were not yet tested in
isolation, but every experiment so far is consistent with the bug living
in how multiple cores' independent timer-driven dispatch loops interact
with the shared scheduler state, not in any individual syscall handler.

## Two more hypotheses checked directly and ruled out this round

- **`sys_sbrk`'s split-critical-section design** (releases `SCHEDULER`
  between validating the request and actually mapping pages, specifically
  so a multi-page grow doesn't stall other cores' scheduling — see that
  function's own doc comment) looked like a plausible target: could a
  cross-core `SYS_KILL` evict a process *during* this gap, mid-mapping-loop,
  with `AddressSpace::drop` then freeing frames out from under a live
  `map_in` call? Traced the actual interrupt-masking discipline instead of
  assuming: `SYSCALL`'s own entry clears `RFLAGS.IF` (`SFMask::write` in
  `syscall::init`), and nothing between entry and the matching `iretq`
  re-enables it (`SpinLock::lock`'s `interrupts_were_enabled` is `false`
  for every lock taken during a syscall, so releasing those locks never
  restores `IF` early). A `RESCHEDULE_VECTOR` IPI targeting a core that's
  mid-syscall therefore queues in the LAPIC and cannot be serviced until
  that syscall's own `iretq` returns to userspace and restores the
  process's own `IF = 1`. `sys_sbrk` cannot be interrupted mid-loop by
  eviction on this architecture — confirmed by reasoning through the
  mechanism, not by testing it fail to reproduce.
- **The recurring `0x3333333333333333` register value** ADR 0015 and 0016
  both flagged as unexplained turns out to have a mundane explanation:
  it is the textbook second-stage mask constant
  (`0x3333333333333333 = u64::MAX / 5`) from the classic branchless SWAR
  population-count algorithm — exactly what LLVM emits for `u64::count_ones()`
  on a plain `x86_64-unknown-none` target with no `+popcnt` feature enabled.
  The only caller, `Bitmap::free_count()` (via
  `memory::phys::free_frame_count()`), is invoked from
  `scheduler::permanent_halt()` — reachable by any individual core the
  instant *it* observes every process gone, independent of whether another
  core is still mid-fault. Both prior captures are consistent with this:
  a real, legitimate intermediate value from ordinary popcount arithmetic,
  not corruption. Recorded here so a future session doesn't re-flag it as
  a lead a third time.

## What remains open

The root cause is still not found. What changed this round is the shape of
the search: this is now known to be a genuine cross-core scheduling/IPI
protocol logic error, reachable via ordinary interleaving (no true
parallelism needed), not exclusively tied to the kill path or the heap
path alone. The most promising next step is a finer-grained slice than
"remove one whole orchestrator": disable the two background pressure
processes (the one piece every other workload runs alongside, purely for
`SYS_YIELD` load, with no IPC/heap/kill semantics of its own) and re-measure
at `-smp 4`, to check whether raw preemption *frequency* alone (independent
of which syscalls are in flight) is the actual variable driving the
failure rate — the scaling data (0% → ~1% → 50%) already looks more
consistent with "more cores means more preemption/dispatch events per
wall-clock second, and the bad sequence needs a certain density of those"
than with any single workload's own correctness.

## Testing

- The four-configuration scaling table above (160 total runs).
- Two 40-run single-workload-removed batches at `-smp 4`.
- Full ~23-scenario regression suite green with the double-fault
  diagnostic addition (the only committed source change this round).
- Both experimental workload-removal changes to `main.rs` were reverted
  before committing; nothing from this section persists in the tree.

## Consequences

- The investigation's own framing changes: future work here should reason
  about interleaving-sensitive logic errors in the scheduling/IPI protocol,
  not hardware-level memory races. Any future fix attempt that only adds
  memory barriers or narrows a lock's scope without changing the actual
  sequence of operations should be treated with suspicion given this
  evidence.
- Two long-unexplained observations from ADR 0015/0016 are now explained
  and retired as leads.
- Root cause remains open. `test-kitchen-sink` stays out of `test-all`/CI.
