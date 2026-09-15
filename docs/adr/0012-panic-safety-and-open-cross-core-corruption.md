# 0012: Panic Safety Across Cores, and an Open Cross-Core Corruption Bug

## Status

Accepted (partial — see Consequences). Implemented across
`kernel/src/task/scheduler.rs`, `kernel/src/arch/x86_64/{lapic,percpu}.rs`,
`kernel/src/lang_items.rs`, `kernel/src/{earlycon,sync}.rs`.

## Context

ADR 0011 shipped forced preemption, the `pending_wake` fix, UART
hardening, and property tests, but deliberately deferred two things to
this milestone: a combined multi-workload stress scenario
(`test-kitchen-sink`, several small processes each driving a different
already-proven-solid subsystem concurrently, plus background scheduling
pressure) and a deeper cross-core issue that surfaced while prototyping
it — one that resisted the same level of investigation ADR 0011's own
two races got.

This milestone picked that up. It found and fixed one more instance of
the kernel-stack-reuse hazard class ADR 0010 first described, and —
more consequentially — found and fixed a completely different class of
bug: a kernel panic can itself deadlock the whole machine, silently,
with no diagnostic output at all. That second finding came from a new
technique for this project: attaching GDB to a *live*, still-running
QEMU instance (via `-monitor unix:...,server,nowait` plus its
`gdbserver` command, enabled at runtime rather than at launch) to read
every core's actual registers and call stack during a hang or a panic,
rather than inferring what happened from the serial log alone. Every
prior milestone's races were diagnosed from log evidence and targeted
assertions; this one needed to see the machine's state directly, because
the log itself was the thing going silent.

The underlying corruption that triggers these panics under
`test-kitchen-sink`'s concurrent load is **not** fully root-caused by
this milestone, despite that investigation. This ADR documents what
*was* found and fixed, what was ruled out and how, and what remains
open — the same shape ADR 0011 used for its own residual
`test-smp-kill-cross-core` failure, and the same shape it used to defer
this milestone's starting point in the first place.

## Decision

### 1. `STACK_BUSY`: closing `finish_switch`'s copy of the ADR 0010 hazard

ADR 0010 fixed kernel-stack reuse for the "outgoing process abandoned,
nothing else ready, go idle" path, by holding `SCHEDULER`'s lock
continuously across the raw stack switch (`abandon_process_stack_and_idle`,
via `core::mem::forget`/`SpinLock::force_unlock`). It never got the same
treatment for the *other* outgoing-process path — `finish_switch`, taken
whenever something else is immediately ready to run — because
`finish_switch` calls `task::executor::run_ready_tasks()` (to drive
kernel tasks like `console_server`), which can itself call back into
`wake_blocked_process`, which needs `SCHEDULER` again; holding the lock
across that call from the same core would self-deadlock, since
`SpinLock` is deliberately non-reentrant.

`STACK_BUSY` (`kernel/src/task/scheduler.rs`) decouples "the table says
this identity is gone" (`Slot::Empty`, needed immediately) from "the
physical stack memory is safe to hand to someone else" (only once
genuinely true), without requiring any lock to be held longer:

- `mark_stack_busy(index)` / `clear_stack_busy(index)`: a lock-free
  `[AtomicBool; MAX_PROCESSES]`, set for a table index the moment its
  owning core relinquishes that identity there (self-exit, fault-kill,
  or a cross-core `SYS_KILL` eviction), cleared only once that core has
  actually finished touching the fixed per-index kernel stack memory —
  `finish_switch`'s own dispatch, `maybe_print_switch`,
  `run_ready_tasks()`, or (for the abandon-and-idle path)
  `idle_loop_trampoline`.
- `allocate_pid()`'s free-slot search now requires `Slot::Empty && !STACK_BUSY[i]`,
  not `Slot::Empty` alone.
- All four call sites of `switch_to_next_or_halt` (`terminate_current_process`,
  `on_reschedule_ipi`, `wait_for_child`'s blocking arm, `block_current_process`)
  now pass the outgoing table index (or `None`, for a pure block/wait
  transition where nothing is being freed) so the busy window can be
  cleared at the right place regardless of which path was taken.

Reproduced, before this fix, as the `x86_64` crate's own internal
`"entry should be mapped at this point"` panic and `SpinLockGuard`'s
`"guard taken before drop"` panic — both reachable only via two
execution contexts genuinely corrupting the same stack memory, never
via a logic error in either.

### 2. `broadcast_panic_halt`: no core is left spinning on a lock a panic orphaned

Caught live via the GDB technique above, investigating a
`test-kitchen-sink` run that looked — from the serial log alone — like
a total, unexplained system freeze with zero output after boot. All
four cores' actual state told a different story:

- One core (via `idt::page_fault_ring0`) had genuinely, correctly
  panicked, and was sitting cleanly in `lang_items::panic`'s own
  terminal `cli; hlt` loop.
- Two other cores were spinning forever inside `task::executor::EXECUTOR`'s
  `SpinLock` (reached from `task::scheduler::on_timer_tick` ->
  `task::executor::run_ready_tasks`).
- A third was spinning forever on `earlycon::COM1_TX_LOCK`.

Both locks were orphaned *forever*: the panicking core had been holding
one or the other at the exact instant its fault struck, and a hardware
fault jumps straight to a new handler without ever running the
interrupted frame's `Drop` — so nothing was ever going to release them.
From outside the guest, this is indistinguishable from a genuine,
unrecoverable hang (no further serial output, and the "high CPU usage"
that would reveal a busy-spin isn't visible to a test runner watching
only the serial port).

The fix (`arch::x86_64::lapic::broadcast_panic_halt`, a new
`PANIC_HALT_VECTOR`) is general, not specific to either lock: the
instant any core recognizes a fatal (ring-0) panic, before doing
anything else — including its own diagnostic print — it broadcasts a
dedicated IPI to every other *booted* core. That IPI's handler
(`panic_halt_handler`) does nothing but `cli; hlt` forever: no EOI, no
lock, nothing that could itself get stuck. A ring-0 panic means the
whole machine is already fatally broken and this core could be holding
*any* lock at all (`COM1_TX_LOCK`, `EXECUTOR`, `SCHEDULER`, ...) — there
is no way to enumerate and fix every lock individually, so the only
general guarantee is to stop every other core from ever needing one
again.

### 3. `panic_println`: the same core can orphan its own lock

The broadcast above only protects *other* cores. Live-caught
separately: a page fault struck one core mid-write of an ordinary,
unrelated `earlyprintln!` call, orphaning `COM1_TX_LOCK` on that exact
core. `panic()`'s own first print then hung on it forever — the
broadcast had already run and stopped every other core, so nothing else
was contending for the lock, but this core's *own* first panic-print
attempt still deadlocked against itself.

`sync::SpinLock` gained `try_lock()` (non-blocking) and `break_lock()`
(forcibly clears the held state regardless of who — if anyone — holds
it; distinct from the existing `force_unlock`, whose contract requires
the caller to actually hold the lock, for the raw-stack-switch case
ADR 0010 introduced). `earlycon::panic_println` — used only by
`lang_items::panic`, never by ordinary `earlyprintln!` call sites —
spins on `try_lock()` up to a bound, then calls `break_lock()` and
proceeds once waiting stops being plausible. Safe specifically because
`COM1_TX_LOCK` only ever guards a raw port write: the worst case of
breaking a lock some other context is still legitimately, briefly
holding is an interleaved/garbled diagnostic line, never memory
unsafety — a strictly better outcome than silence forever.

## Real bugs found (via live GDB inspection and adversarial stress-testing)

1. **`finish_switch`'s kernel-stack-reuse window** — the same hazard
   class ADR 0010 fixed for the abandon-and-idle path, unfixed for the
   fast-dispatch path. Fixed by `STACK_BUSY`.
2. **Cross-core orphaned-lock deadlock on panic** — any core's fatal
   panic could leave *any* lock it held permanently stuck, hanging
   every other core that later needed it, indistinguishable from a
   total system freeze. Fixed by `broadcast_panic_halt`.
3. **Same-core orphaned-lock self-deadlock on panic** — the panicking
   core's own first diagnostic print could hang on a lock it had
   itself just orphaned. Fixed by `panic_println`/`break_lock`.

## What remains open: the actual `test-kitchen-sink` corruption

After both fixes above, `test-kitchen-sink` still fails roughly
65-70% of runs — **no better than before** in raw pass rate — but its
failures changed shape completely. Before: a large fraction were silent,
zero-output hangs, indistinguishable from a dead machine. After, across
several 20-30-run before/after batches: every failure is now either a
complete panic message ending in a clean `halting.`, an explicit
`KS_*_FAIL` marker, or a specific "never reported a result" timeout —
never silence. The underlying corruption is still real and still
unfound, but it is now always diagnosable, which is the precondition
for anyone (this milestone or a future one) actually finding it.

**Ruled out this milestone, with evidence, not just suspicion:**

- *The specific `finish_switch` stack-reuse race is not the (sole)
  cause* — `STACK_BUSY` closes a real, reproduced hazard, but the
  overall failure rate was statistically unchanged after fixing it.
  Some fraction of past failures were almost certainly this bug; most
  of the remaining ones are something else.
- *Mid-syscall cross-core eviction corrupting an unlocked window*
  (e.g. `sys_sbrk`'s split critical section) — checked directly against
  the code, not just tested: `context_switch::ring0_reschedule`
  deliberately does nothing when a `SYS_KILL` eviction IPI lands while
  the target is at ring 0 (mid-syscall), leaving `evict_request` set;
  `terminate_process`'s spin-wait times out, and its outer loop
  re-scans and re-targets once the process safely returns to ring 3 or
  blocks. A process is never evicted out from under its own kernel-mode
  execution.
- *Ongoing kernel-half page-table mutation without TLB shootdown as a
  live trigger* — a real, still-deferred gap since ADR 0009's own
  scope, but not one with an active trigger during
  `test-kitchen-sink`: the kernel heap is a fixed-size region mapped
  once at boot (`memory::heap::init`, no growth path exists yet), and
  the LAPIC's MMIO mapping happens once, BSP-only, before any AP
  starts. Nothing remaps kernel-half memory while the test runs.
- *QEMU MTTCG host-thread races* — ruled out in an earlier investigating
  session by reproducing the same failure signatures under
  `-accel tcg,thread=single` (fully serialized emulation).
- *A stale ready-queue entry surviving a slot reuse*, and *a corrupted
  `Process.trap_frame` reaching `switch_to` already broken* — both
  guarded by permanent, defense-in-depth assertions in `switch_to`
  (generation check; `cs`/`rip` sanity check) that have never fired
  across extensive stress-testing, including this milestone's, despite
  panics still occurring elsewhere.

**Still open, with evidence pointing at a specific shape:**

- Ring-0 panics show data that looks *corrupted in memory*, not merely
  "a process did something invalid": one page fault's own trap frame
  reported an instruction-fetch at an address matching a real kernel
  `.bss` symbol's low 48 bits, but with the top byte zeroed instead of
  the expected `0xff`; another reported a page-fault error code with
  `SGX`/`RMP`/`PROTECTION_KEY` bits simultaneously set — a combination
  no genuine QEMU/TCG hardware would ever produce. Both look like a
  stack or heap location that held a valid value when written and a
  garbled one by the time our fault handler read it back, rather than
  a value that was always wrong.
- `heap-child` — a real userland program (`userland/heap-child`), not a
  kernel-internal dummy process — once reported `KS_HEAP_FAIL` cleanly:
  its own `Vec<u64>` content check found a mismatch after growing via
  dozens of separate `sys_sbrk` calls under concurrent load. This is
  the most tractable open lead of everything found this milestone: a
  real, userspace-visible corruption with a known reproduction shape
  (`test-kitchen-sink`'s concurrent pressure), rather than an opaque
  kernel-side panic with no user-visible counterpart to cross-check
  against.

## Testing

- Three separate 20-30-run before/after stress batches of
  `test-kitchen-sink`, comparing failure signatures across each fix
  (`STACK_BUSY` alone; `+broadcast_panic_halt`; `+panic_println`).
- Live GDB inspection of a running QEMU instance (gdbstub armed
  at runtime via the monitor socket's `gdbserver` command) during both
  a caught hang and a caught panic — this milestone's key new technique,
  necessary because the log itself was the thing going silent.
- Full ~23-scenario regression suite (`xtask test-all`), run twice,
  both green, confirming none of this milestone's changes regressed
  anything Milestones 1-8 already verified.
- `cargo test -p tarnos-kcore -p tarnos-abi` and
  `cargo clippy -p tarnos-kcore -p tarnos-abi --all-targets -- -D warnings`,
  both clean.
- `test-kitchen-sink` deliberately still not wired into `test-all`/CI
  (see `kernel/Cargo.toml`'s own comment) — its ~30% pass rate is not
  yet reliable enough.

## Consequences

- **Milestone 9's original goal — get `test-kitchen-sink` passing
  reliably and wire it into CI — is not met.** This ADR closes what
  this milestone *did* accomplish and documents the remaining
  corruption's evidence as a concrete starting point for whichever
  milestone picks it up next, the same way ADR 0011 documented its own
  residual `test-smp-kill-cross-core` failure rather than leaving it
  unrecorded.
- Two previously-invisible classes of deadlock — orphaned-lock-on-panic,
  both cross-core and same-core — are fixed generally, for every future
  panic this kernel will ever have, not just this one scenario. This is
  a real, standalone hardening win independent of whether the
  underlying corruption is ever found: any future bug that panics under
  load now reports itself instead of silently wedging the machine.
- Live GDB-based inspection of a running QEMU instance is now a proven,
  repeatable technique for this project — worth reaching for directly
  in future cross-core investigations instead of defaulting to
  log-and-assertion-based inference alone, especially when the log
  itself might be the thing that goes silent.
- `test-kitchen-sink` stays out of `test-all`/CI, unchanged from
  Milestone 8's own deferral of it.
- Keyboard drivers, filesystems, and the rest of the longer-term
  roadmap remain explicitly what finishing this corruption investigation
  exists to precede, not begin.
