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

Four temporary, uncommitted experiments (spawning `test-kitchen-sink`'s
process set with some workload's spawn code deleted, then reverted) at
`-smp 4`, 40 runs each, counting genuine `[KERNEL PANIC]` occurrences only:

- **Kill orchestrator removed:** 10/40 panicked (25%).
- **Heap orchestrator removed:** 7/40 panicked (17.5%).
- **Both background `SYS_YIELD`-pressure processes removed** (IPC, heap,
  lifecycle, and kill orchestrators all still present): 6/40 panicked
  (15%) — pressure alone contributes about as much as heap does, and
  removing it still leaves a substantial rate.
- **Kill, heap, *and* pressure all removed** — only the IPC round-trip
  and lifecycle spawn/wait orchestrators (plus their spawned
  `echo-child`/`exit-code-child` children) still running: 9/40 panicked
  (22.5%), squarely in the same range as every partial configuration
  above.

No removal, including stacking three of them together, comes close to
eliminating the panics, and no single workload's share stands out as
dominant — every configuration tested lands in the same 15–25% band
regardless of which specific syscalls are in flight. `SYS_SPAWN`,
`SYS_GRANT`, `SYS_PROCESS_START`, `SYS_WAIT`, and `SYS_SEND`/`SYS_RECV`
are the only syscalls the surviving IPC+lifecycle configuration still
exercises — `SYS_KILL`'s cross-core eviction protocol and `SYS_SBRK`'s
split-critical-section growth path are both completely absent from that
run, yet the panic rate barely moved. This rules out a defect confined to
any individual syscall handler and confirms the one thing every
configuration in this table still has in common regardless of which
workloads survive: the scheduler's own per-core dispatch loop
(`scheduler::on_timer_tick` → `switch_to`, driven by every core's
independent LAPIC timer against the one shared ready queue) plus the
ordinary spawn/wait/wake machinery every syscall here ultimately funnels
through (`scheduler::spawn`/`wait_for_child`/`wake_blocked_process`).
The search should now focus there, not on any workload-specific code
path.

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

The root cause is still not found, but the search space is now about as
narrow as workload-isolation alone can make it: with kill, heap, and
pressure all removed, the smallest configuration tested (IPC round-trip +
lifecycle spawn/wait, four short-lived orchestrator/child processes total)
still panics at close to half the full scenario's own rate. Further
subtracting individual workloads is unlikely to isolate this any further —
the next step has to be looking *inside* the shared dispatch/wake path
itself (`on_timer_tick`, `switch_to`, `spawn`, `wait_for_child`,
`wake_blocked_process_locked`/`block_current_process_locked`) rather than
removing more of what surrounds it. Candidate approach for next session:
instrument `switch_to`'s existing double-dispatch assertion and
generation check (already present as defense-in-depth, never yet observed
to fire) with a ring buffer of the last N transitions per core — pid,
generation, and a monotonic sequence number — captured *unconditionally*,
so a panic anywhere can dump exactly what every core's dispatch loop did
in the instants leading up to it, rather than relying on the fault's own
core to have caught something informative. The scaling data (0% → ~1% →
50%) already looks more consistent with "more cores means more
preemption/dispatch events per wall-clock second, and the bad sequence
needs a certain density of those" than with any single workload's own
correctness — a per-core trace buffer is the most direct way to actually
see that sequence instead of continuing to infer its shape indirectly.

## Testing

- The four-configuration scaling table above (160 total runs).
- Four 40-run workload-removal batches at `-smp 4` (160 more runs).
- Full ~23-scenario regression suite green with the double-fault
  diagnostic addition (the only committed source change this round).
- All four experimental workload-removal changes to `main.rs` were
  reverted before committing; nothing from this section persists in the
  tree.

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
