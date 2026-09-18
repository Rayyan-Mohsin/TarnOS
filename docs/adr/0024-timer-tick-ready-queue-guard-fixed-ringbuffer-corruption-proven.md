# 0024: on_timer_tick's Ready-Queue Guard Fixed; RingBuffer Corruption Proves a Genuine Wild Write

## Status

Accepted. A second real, permanent correctness bug fixed this round
(distinct from ADR 0023's panic-reporter race). Root cause of the
underlying cross-core corruption remains open, but this round produced
the cleanest possible proof yet that the mechanism is a genuine wild
write to scheduler memory, not a logic bug in any single subsystem
already audited.

## Context

ADR 0023 closed by naming the next concrete lead: audit every path
that can put a `Pid` into `sched.ready` or hand one to `switch_to` for
a case where the value itself, not the table it's checked against, is
wrong. `spawn`, `start_child`, `wake_blocked_process_locked`, and
`block_current_process_locked` were re-read against that question.

## Found and fixed: on_timer_tick's asymmetric ready-queue push

Three of those four call sites gate their `sched.ready.push(..)` on
first confirming the process's slot is genuinely `Slot::Occupied`
(`occupied_mut`'s check, or an equivalent inline match). `on_timer_tick`
did not:

```rust
if let Some(current_pid) = sched.current[core] {
    percpu::slot(core).preempt_count.fetch_add(1, Ordering::Relaxed);
    if let Slot::Occupied(process) = &mut sched.processes[current_pid.index()] {
        process.trap_frame = unsafe { *current_frame };
        process.state = ProcessState::Ready;
    }
    sched.ready.push(current_pid);   // <-- unconditional
}
```

If `sched.processes[current_pid.index()]` were ever not `Occupied` —
for any reason, a legitimate cross-core kill race or the still-open
corruption this investigation has been chasing — the inner `if let`
silently skips saving the trap frame, but the function still pushed
`current_pid` back into the ready queue regardless. This matters more
than it would in an ordinary bug list: `on_syscall_yield` is a bare
alias for `on_timer_tick` (`pub fn on_syscall_yield(current_frame: *mut
TrapFrame) -> *mut TrapFrame { on_timer_tick(current_frame) }`), so
*every* `SYS_YIELD` in the pure-yield reproduction — its only workload
— goes through this exact code. This was the one place in the whole
dispatch path where a `current[core]` that no longer named a live
process could be handed straight back to `switch_to` instead of being
dropped, exactly the propagation shape needed to explain ADR 0023's
clean "phantom pid" capture (`switch_to named a process that does not
exist`).

### The fix

Moved the push inside the `Slot::Occupied` match, matching the pattern
already used everywhere else in this file:

```rust
if let Slot::Occupied(process) = &mut sched.processes[current_pid.index()] {
    process.trap_frame = unsafe { *current_frame };
    process.state = ProcessState::Ready;
    sched.ready.push(current_pid);
}
```

A stale `current_pid` is now silently dropped rather than re-queued —
consistent with every other call site's existing behavior for this
exact scenario (none of them assert either; all four already treat "not
Occupied" as "nothing to do here"), not a new, inconsistent recovery
policy invented for this one call site.

## Result: no recurrence of the phantom-pid panic, but new proof of a wild write

A 40-run pure-yield batch (`-smp 4`, 17s timeout) with this fix active
did not reproduce ADR 0023's "does not exist" panic, nor any
`switch_to` hardening assertion — consistent with (not proof of) the
fix closing that specific propagation path. Panic rate stayed in this
investigation's usual noisy range (31/40), confirming this fix narrows
one path rather than eliminating the underlying corruption.

One new capture in that batch is the single cleanest piece of evidence
this whole investigation has produced:
`libs/tarnos-kcore/src/ring.rs:64:21: index out of bounds: the len is
16 but the index is <large>` — inside `RingBuffer::pop`'s
`self.buffer[self.head].take()`. `RingBuffer`'s own code has exactly
two places that ever touch `head`: initialization to `0`, and
`self.head = (self.head + 1) % N` — both of which make `head >= N`
architecturally impossible through the type's own logic alone (already
proptest-verified against a `VecDeque` reference model, ADR 0020/0021).
The only way `head` can hold an out-of-range value is a write to that
exact memory that did not go through either method — i.e., a genuine
wild write landing on `sched.ready`'s own backing memory from
somewhere else in the kernel, external to `RingBuffer`'s own,
already-correct implementation. This is stronger than every prior
"garbled value" observation in this investigation (a wrong `Pid`, a
wrong core index, an impossible `PageFaultErrorCode`): those were all
consistent with corruption, but none of them ruled out a same-type
logic error as cleanly as this one does. `Inner`'s field layout is not
`#[repr(C)]`, so no claim is made about *which* adjacent write caused
it — only that some write, somewhere, is landing on memory it has no
business touching.

## Testing

- `cargo run -p xtask -- build`: clean.
- Full 22-scenario regression suite: green, confirming the ready-queue
  guard doesn't change behavior on any correct-path scenario, including
  the cross-core kill/forced-preempt scenarios most likely to
  legitimately exercise the "not Occupied" branch this fix touches.
- Pure-yield reproduction, `-smp 4`, 17s timeout, 40 runs: 31/40 total
  panics; zero recurrences of ADR 0023's "does not exist" panic or any
  `switch_to` assertion; one new, structurally decisive `RingBuffer`
  bounds-check capture.
- Every ISO build verified via its own boot-log line and
  `strings ... | grep -c "pure-yield process"` before trusting a batch
  against it.

## Consequences

- `on_timer_tick`'s ready-queue guard is a permanent, independent
  correctness fix — not a temporary experiment — matching this file's
  own established invariant (never re-queue a pid without first
  confirming its slot is still genuinely occupied by it).
- The corruption's mechanism is now proven, not merely suspected, to
  be a wild write into scheduler-owned memory rather than a logic
  error reachable through any already-audited subsystem's own public
  API. `RingBuffer`, `sched.current[]`, `sched.generations[]`,
  `process.trap_frame`, and `process.address_space` have each now
  individually shown symptoms consistent with exactly this same
  mechanism, across this and prior rounds.
- The most productive next step is a systematic audit of every `unsafe`
  block anywhere in the kernel that computes a *write* address from a
  value that could itself be wrong under adversarial load — `core_index()`-
  derived per-core array writes in particular, since a write (not a
  read) through a corrupted index is the one mechanism that could
  explain corruption landing on arbitrary, otherwise-unrelated kernel
  memory rather than only being observed when that exact memory is
  later read back.
- `test-kitchen-sink` stays out of `test-all`/CI. Root cause of the
  underlying corruption remains open.
