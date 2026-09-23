# 0027: The Whole `Process` Struct Guard-Paged; a Fourth Null Result, an Unexplained Rate Drop, and a Stack-Resident Capture

## Status

Accepted. Permanent hardening added (kept regardless of outcome, same
reasoning as ADR 0025/0026). The guard pages around `Process`'s own
backing memory came back clean across two 40-run batches, closing the
last remaining "does a wild write overshoot into this specific region"
question this technique can usefully ask. Two things complicate a clean
writeup, and both are reported honestly rather than smoothed over: the
overall corruption rate dropped substantially and unexplained, and a
new, differently-shaped capture — corruption reaching a *stack-resident*
`SpinLockGuard`, not heap-allocated scheduler/process state — surfaced
in code this round never touched.

## Context

ADR 0026 guard-paged `Process::trap_frame`'s own *target* memory and
came back clean, but decisively proved something else: the *stored
reference itself*, sitting inside `Process`'s own `Box` allocation on
the general kernel heap, reads back as null while the process is live.
That ADR's own Consequences named the natural next test directly:
guard-page the whole `Box<Process>` allocation, not just one of its
fields, to test whether a wild write reaches that allocation from
outside it (the general heap, unlike every region guarded so far, has
no gap between neighboring allocations).

## Implementation: `ProcessBox`

`Box<Process>` is replaced everywhere by a new type, `ProcessBox`,
implementing the same `Deref`/`DerefMut`/`Drop` shape so every existing
call site (`Slot::Occupied(process)`, `process.trap_frame`,
`pending_drops.push(process)`, etc.) needed no change beyond the type
name itself. The difference is entirely in where its backing memory
lives: one of 32 (`PROCESS_SLOT_COUNT = MAX_PROCESSES * 2`) fixed,
guard-paged slots (`PROCESS_BOX_BASE = 0xffff_9770_...`), mapped eagerly
at boot (`task::scheduler::init_process_slots`, wired into `main.rs`
alongside the other three `init_*` calls this investigation has already
added, same ordering requirement), rather than an arbitrary spot on the
general heap.

Twice `MAX_PROCESSES`, not once: unlike `trap_frame`'s simpler
per-table-index reuse (safe because a `TrapFrame` is `Copy` and holds no
resources), a terminating process's `ProcessBox` is extracted into
`pending_drops` and its *table index* freed for a new occupant *before*
the old `ProcessBox` itself is actually dropped (deferred until this
core's CR3 has moved off the old process's `AddressSpace` — see
`terminate_slot`). So a fresh process can occupy the same table index
while its predecessor's `ProcessBox` is still alive, undropped, elsewhere
— a genuine allocation, not merely a stack borrow — so backing storage
can't simply be "one slot per table index, reused immediately" the way
`trap_frame`'s is. A small free-list (`PROCESS_SLOT_FREE`, its own
ordinary, un-guarded `.bss` bitmap behind its own dedicated lock — not
`SCHEDULER`'s, since `ProcessBox::drop` runs from
`idle_loop_trampoline`, after `SCHEDULER` has already been released)
hands out and reclaims slots; `ProcessBox::new` writes fresh into
whichever slot it's handed, `Drop` runs `ptr::drop_in_place` (so
`AddressSpace`'s own `Drop` still frees its PML4 exactly as it did
inside a real `Box`) before returning the slot.

`map_guarded`'s own page-mapping logic was factored out into
`map_guarded_raw` (maps pages, writes nothing) so `init_process_slots`
could map all 32 slots without needing a placeholder `Process` value —
unlike `trap_frame`'s harmless all-zero `TrapFrame`, a placeholder
`Process` would need a real physical frame for its `AddressSpace` just
to stay droppable correctly, wastefully, for slots sitting unused most
of the time.

## Result 1: a fourth clean null result

Two 40-run batches (`-smp 4`, 17s timeout, full `kitchen-sink-test`
workload — five orchestrators plus pressure processes, real spawn/IPC/
heap/kill activity, **not** the reduced 8-process pure-yield
reproduction; see Consequences for a process note on why that
distinction mattered this round) produced 14/40 and 18/40 panics.
Across both batches, not one fell inside or adjacent to the new
`PROCESS_BOX_BASE` region. One general-protection-fault capture's
printed *error code* (not its faulting address) happened to start with
digits resembling `SCHED_STATE_BASE` (`0xffff9700...`) — noted and
dismissed: a GP fault's error code is a small selector-derived value,
not a memory address that was actually accessed, and the same capture's
real faulting address was an ordinary kernel-stack-slot address, the
established pattern since ADR 0018. This is the fourth consecutive
clean result for "a wild write overshoots into this specific guarded
region," now covering the scheduler's whole state, its fields
individually, `trap_frame`'s own target memory, and now the whole
`Process` struct's own backing storage.

## Result 2: a large, unexplained drop in overall corruption rate

The combined rate this round — 32/80 (40%) — is roughly half ADR 0026's
own two batches against the identical workload and timeout (32/40 and
36/40, 80-90%). This is reported as an open, unexplained observation,
not a claimed fix: every captured panic this round shows the exact same
signature catalogue this investigation has tracked since ADR 0018/0019
(a `rip`/`rsp` landing inside some kernel-stack slot, tiny garbage
addresses like `0x1`/`0x2`/`0x180`, a corrupted core index of `384`, the
`core::sync::atomic` "no such thing as an acquire-release failure
ordering" panic from a corrupted `Ordering` value at a `compare_exchange`
call site). If `ProcessBox` had actually closed the underlying bug, a
materially different failure distribution (or none at all) would be the
expected signature — not the same catalogue at roughly half the rate.
The more likely explanation is that this refactor perturbed timing
enough to change how often the still-unknown race's window gets hit
(added a slot-bitmap lock acquisition and linear scan on every spawn,
changed the guard-paged memory's layout and TLB/cache behavior) without
touching its root cause — races are notoriously sensitive to exactly
this kind of unrelated timing change in either direction. Host-machine
scheduling noise between separate sandbox sessions is a second,
un-ruled-out candidate. Neither is confirmed; both are more plausible
than "guard-paging `Process` fixed a use-after-free," which the
identical failure catalogue argues against directly.

## Result 3: a new, stack-resident capture in code this round never touched

The second batch's own captures include one new shape:
`kernel/src/sync.rs:135:29: guard taken before drop` —
`SpinLockGuard::deref`'s own `self.guard.as_ref().expect(...)` firing
because the `Option<MutexGuard<T>>` field backing a live
`SpinLockGuard` read back as `None`. `sync.rs` was not touched this
round (or any prior round of this investigation). This matters because
a `SpinLockGuard` is an ordinary local variable, normally living on
whichever kernel stack currently holds it — not a heap allocation like
every structure guard-paged so far (`Inner`'s fields, `trap_frame`,
`Process` itself). This is the first capture in this entire
investigation (ADR 0012 onward) that directly names corruption reaching
a *stack-resident* value's own internal state, rather than being
inferred indirectly from a corrupted `rsp`/`rip` landing on a
kernel-stack address. It broadens, rather than narrows, the search: the
mechanism can apparently reach heap and stack alike, which reads more
like a wild write (or corrupted pointer used for one) or a stray
register/stack-frame clobber from somewhere in the interrupt/
context-switch path, than a defect specific to any one data structure's
own logic.

## Testing

- `cargo run -p xtask -- build`: clean, `ProcessBox` refactor compiled
  correctly on the first attempt (careful upfront call-site audit before
  building, same discipline as ADR 0026's `trap_frame` refactor).
- Full 22-scenario regression suite: green, run twice (once before the
  stress batches, once again immediately before this commit).
- Two 40-run, full `kitchen-sink-test` workload batches (`-smp 4`, 17s
  timeout): 14/40 and 18/40 panics, zero hits in the new guarded region
  either time.
- Every ISO build verified via its own boot-log line
  (`[boot] kitchen-sink-test: spawned all orchestrator + pressure
  processes`) before trusting a batch against it.

## Consequences

- The guard-paged `ProcessBox` relocation is permanent, kept regardless
  of this round's own null result on its primary question — same
  reasoning as ADR 0025/0026's guarded regions.
- **Process note**: this round's own temporary `main.rs` experiment was
  initially built using the *reduced 8-process pure-yield*
  reproduction (ADR 0019's original minimal repro), based on a
  compressed summary of this investigation's own history that
  conflated it with the workload actually used in ADR 0026's own
  batches. Direct inspection of ADR 0026's own saved logs
  (`trapframe_exp40/*.log`) showed its actual boot marker was
  `[boot] kitchen-sink-test: spawned all orchestrator + pressure
  processes` — the full workload, not the pure-yield one — and for
  good reason: the pure-yield reproduction never spawns or kills a
  process after boot, so it cannot exercise `ProcessBox`'s own
  alloc/free path at all, the exact thing this round's experiment
  needed to stress. Caught before any stress batch was trusted, by
  checking the actual prior logs rather than the summarized text
  describing them; the mislabeled pure-yield batch was kept on disk
  under an explicit `_MISLABELED` suffix rather than discarded, and the
  correct full-workload batches were used for every result reported
  above. Worth carrying forward as its own durable note: when
  reproducing a prior round's methodology, verify it against that
  round's own saved raw logs, not a summarized description of them.
- Four rounds of guard-paging (scheduler state at two granularities,
  `trap_frame`'s target memory, and now `Process`'s own allocation) have
  now consistently found nothing at any boundary tested. This technique
  has answered the question it's good at asking — "does a wild write
  overshoot from outside into this specific region" — for every
  major piece of scheduler/process state, and the answer has been no
  each time. Continuing to guard-page yet more structures looks like
  diminishing returns. This round's two new findings point elsewhere
  instead: the unexplained rate sensitivity to unrelated timing changes,
  and directly-observed stack-resident corruption
  (`sync.rs`'s `SpinLockGuard`) suggest the next step should look at
  the interrupt-entry/context-switch path's own register and stack-frame
  handling directly, rather than at any one more data structure.
- `test-kitchen-sink` stays out of `test-all`/CI. Root cause of the
  underlying corruption remains open.
