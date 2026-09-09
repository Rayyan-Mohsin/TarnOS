# 0002: Async Executor and Scheduler as Two Run-Loops, One Bridge

## Status

Accepted. Implemented in `kernel/src/task/executor.rs`,
`kernel/src/task/scheduler.rs`, `kernel/src/sync.rs`.

## Context

TarnOS needs to run two very different kinds of "task" concurrently:

- **Untrusted usermode processes** (`init`, and everything after it).
  These must be force-preemptible — a process cannot be trusted to yield
  voluntarily, so the only safe design is a timer interrupt that can rip
  control away from it at any instruction boundary and save/restore a
  full register + stack context.
- **Trusted, stackless kernel-internal work** (the UART RX-echo task, the
  console server). These are futures driven by a `Waker`, not full
  threads — they have no separate stack to save, and since the kernel
  itself is trusted code, there is no isolation reason to force-preempt
  them the same heavyweight way.

Building one unified run-loop for both would mean giving every kernel
task the full weight of a process context switch (its own kernel stack,
`TrapFrame`, address-space entry in the process table) for work that is
really just "poll this future when it's woken" — and would mean the
process scheduler's hot path (timer tick, interrupts disabled) needing to
understand `Future`/`Waker` machinery that can allocate.

The other forcing constraint: [`on_timer_tick`] and any interrupt handler
run with interrupts disabled and must never touch the global heap
allocator's lock, because that lock is not interrupt-reentrant — code
elsewhere holding it when the timer fires, followed by the timer handler
trying to allocate too, would deadlock the core against itself.

## Decision

**Two separate run-loops, one bridge primitive.**

- `task::scheduler` is a preemptible round-robin scheduler over
  *processes*. Its process table and ready queue are fixed-capacity
  arrays (`MAX_PROCESSES = 16`), not `BTreeMap`/`VecDeque` — deliberately
  a deviation from the original plan sketch, adopted once it became clear
  `on_timer_tick` runs in exactly the disabled-interrupt, no-allocation
  context above. The only place the scheduler's state allocates
  (`Box::new` in [`spawn`]) is process *creation*, which only ever runs
  from normal code, never from an interrupt handler.
- `task::executor` is a cooperative executor over *kernel futures*, global
  (`static EXECUTOR: SpinLock<Executor>`) rather than one instance per
  call site, because kernel tasks need to keep making progress from two
  different places: the kernel's own boot-time idle loop, and — once real
  processes exist and `scheduler::start()` never returns to that idle
  loop — the scheduler's own per-tick drain (`on_timer_tick` calls
  `executor::run_ready_tasks()` before returning). A value reachable from
  only one of those call sites couldn't be reached from the other. Its
  ready queue is a fixed-capacity ring buffer (`READY_QUEUE_CAPACITY =
  64`) for the same allocation-free reason as the scheduler's process
  table — an interrupt handler's `Waker::wake()` call must be able to
  push a ready task ID without ever allocating.
- **The bridge, and the load-bearing rule that makes it safe:** an
  interrupt handler may only ever *enqueue* — call `Waker::wake()` (which
  pushes a `TaskId` into the ready-queue ring buffer) or push a process
  `Pid` into the scheduler's ready queue. It may never poll a future or
  run process code inline. Actual polling only happens at two kinds of
  checkpoint: the timer tick (already interrupts-disabled, already not
  reentering itself) and syscall-return-to-usermode. This is what lets
  the ready queue be a plain array instead of anything backed by the
  allocator.
- `sync::SpinLock<T>` — a spinlock that disables interrupts for the
  duration it's held — is the one primitive used by *any* state touched
  from both normal code and an interrupt handler (the executor's task
  map, a driver's registered `Waker`, the PIC). A plain `spin::Mutex`
  would allow the reentrancy hazard this bridge design otherwise
  eliminates by construction: normal code holding a plain mutex,
  interrupted by a handler that wants the same lock, deadlocks the core
  against itself since there's no second core to make progress. This
  class of bug was not hypothetical — it caused a real, intermittently
  reproducing (~1-in-3) UEFI boot hang when the PIC's own lock
  (`arch::x86_64::interrupts::PICS`) was left as a plain `spin::Mutex`
  despite being touched from both an interrupt handler's EOI and normal
  code's `unmask_irq4()`; see the git history for the fix and the
  `xtask test-fault` regression test this milestone added.

## Consequences

- Kernel tasks never get their own kernel stack or process-table entry —
  cheap to spawn, cheap to poll, at the cost of never being able to block
  on something that isn't expressible as a `Future` (no blocking syscalls
  from kernel-task context).
- Every future addition of state shared between normal code and an
  interrupt handler must use `sync::SpinLock`, never a plain mutex — this
  is now a checked-by-convention invariant (see the doc comments on
  `driver::IRQ_TABLE` and `arch::x86_64::interrupts::PICS`), not enforced
  by the type system. A future contribution guideline or lint could catch
  violations mechanically; none exists yet.
- The two run-loops are not yet time-sliced against each other in a
  unified sense — kernel tasks get a slice of every timer tick regardless
  of process load, and there's no priority or fairness scheme between
  "kernel task work" and "process work." Acceptable for one echo task and
  one console server; will need revisiting if kernel-task work ever
  becomes heavy enough to starve process scheduling.
- Once `scheduler::start()` hands control to ring-3 processes, the
  kernel's original boot-time idle loop never runs again — kernel tasks
  are only kept alive by the scheduler's per-tick and per-exit drains
  added specifically to cover this handoff. If a future milestone adds a
  path where the scheduler could stop calling `run_ready_tasks()` (e.g. a
  different idle strategy), kernel tasks would silently stop progressing;
  this dependency should be kept explicit wherever the scheduler's
  control flow changes.
