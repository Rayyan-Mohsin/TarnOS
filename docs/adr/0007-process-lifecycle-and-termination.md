# 0007: Process Lifecycle and Termination

## Status

Accepted. Implemented across `kernel/src/task/{scheduler,process}.rs`,
`kernel/src/memory/virt.rs`, `kernel/src/arch/x86_64/syscall.rs`, and
`libs/tarnos-abi/src/lib.rs`.

## Context

Milestone 3 added dynamic process creation and named three real gaps as
follow-ups, all of which only became *reachable* once processes could
actually be created and destroyed at runtime instead of once at boot:

- **A physical memory leak on every process exit.** Nothing in the
  kernel implemented `Drop`; a terminated process's `Box<Process>` was
  simply dropped-in-place (`sched.processes[i] = None`), which frees the
  Rust heap allocation for the `Process` struct itself but does nothing
  to return the physical frames backing its `AddressSpace` — its page
  tables or its mapped pages — to the frame allocator.
- **No exit-notification/wait mechanism.** `sys_exit`'s exit code was
  computed by userland, placed in a register, and then never read by
  the kernel at all. A parent had no way to learn a child exited, or
  how, beyond a voluntary application-level IPC reply.
- **No way to stop a stuck or misbehaving process from outside**, and
  `Pid` had no generation counter — `allocate_pid`'s slot-reuse (added
  in Milestone 3) was only sound *because* nothing could kill a
  `Blocked`/`Suspended` process externally.

This milestone closes all three together — they turned out to be
tightly coupled (exit notification needs something to survive briefly
after a process "ends"; the kill syscall needs the Pid-reuse safety
fix; reaping orphaned children needs both).

## Decision

### One shared primitive underneath self-exit, fault-kill, and `SYS_KILL`

`task::scheduler::terminate_slot(pid, status)` is now the single place
a process's life actually ends. It extracts the process, removes it
from the ready queue if present, resolves anything blocked waiting on
it, finalizes its table slot, sweeps its own leftover children, and —
critically — drops its `Box<Process>` only *after* releasing the
scheduler lock, since `AddressSpace`'s teardown (below) is a
variable-length walk this codebase's convention keeps off that lock's
critical path (the same rule `resolve_endpoint`'s and `sys_grant`'s doc
comments already state). `terminate_current_process` (self-exit,
fault-kill) and the new `terminate_process` (`SYS_KILL`) are both thin
callers of it.

### `AddressSpace` frees its own frames via `Drop`, not a call-and-forget function

A recursive page-table walker (`memory::virt::free_table_subtree`)
frees every frame an `AddressSpace` owns: starting from each present
PML4 entry in indices `0..256` (the user half — indices `256..512`, the
kernel half every address space shares by copying PML4 entries at
construction, are never touched or recursed into, so the heap, every
process's kernel stack, and the double-fault stack stay structurally
unreachable from this walk), it recurses PDPT → PD → PT, and — the bug
that mattered most — treats a PT's own entries as **leaf data frames to
free directly**, not as pointers to further tables. An earlier version
of this walker got that last step wrong: it only recursed when
`level > 1`, which correctly freed every intermediate PDPT/PD/PT frame
but never so much as looked at a PT's 512 entries, silently leaking
every actual mapped page (every ELF segment, every user stack) on every
single process exit. Caught by `xtask test-process-lifecycle` asserting
the physical frame allocator's free-frame count returns to its exact
starting value after 48 spawn+kill cycles — it did not, until this was
fixed.

This is `impl Drop for AddressSpace`, not a manually-called
`destroy()`: it can't be forgotten, and it fixes a second, pre-existing
bug for free — every early `?`-return in `Process::from_elf`/`new`
(partial ELF load failure, OOM mapping the user stack) already leaked a
partially-built `AddressSpace` with zero cleanup, since nothing ever
called a cleanup function on that path either.

### `Process::new_dummy` had to switch from remapping to copying

Making `AddressSpace::drop` real surfaced a second, more subtle bug in
existing Milestone 2/3 test infrastructure: `new_dummy` (the mechanism
behind every raw-asm dummy test process) worked by remapping the
*kernel's own* `.text` frame containing a compiled function as
user-executable. Once process exit genuinely frees every leaf frame it
finds mapped, a dummy process exiting would free that live kernel code
frame back to the allocator — silent corruption waiting to happen the
next time that frame got handed out for something else and written to.
`new_dummy` now allocates fresh, process-owned frames and copies the
function's compiled bytes into them (safe, since these dummy functions
use only PC-relative jumps and `const`-baked-as-immediate operands, no
addressing that depends on their original load location) — the process
now genuinely owns what it frees. It also now copies two consecutive
pages, not one: a function's start offset within its original page is
never guaranteed page-aligned, so a long-enough function's tail can
spill into the next page regardless of size — a second, independent bug
`test-process-lifecycle` hit immediately (an instruction-fetch page
fault) before the frame-leak issue was even visible.

### Exit status + `SYS_WAIT`, with the process table gaining a third state

`Inner::processes`'s element type became a three-way `Slot`: `Empty`,
`Occupied(Box<Process>)`, or `Zombie { parent: Pid, status: ExitStatus }`.
`parent` here is never optional — a slot only ever becomes a `Zombie`
when someone could legitimately reap it; a process with no parent
(everything boot-created) goes straight to `Empty`, exactly preserving
prior behavior. `ExitStatus` (`Exited(i32) | Faulted | Killed`, new in
`tarnos-abi`) is what `on_syscall_exit` now actually builds from the
exit-code register it previously ignored.

`SYS_WAIT(target_pid)` waits on one *named* child only, not "wait for
any child" — deliberately: the permission model (only `target.parent`
may ever wait on it) means at most one process can ever legitimately be
waiting on a given target, so a single `wait_waiter: Option<Pid>` field
on `Process` suffices; no wait queue is needed. Blocking reuses the
existing `ProcessState::Blocked`/`block_current_process` machinery
exactly as `sys_send`/`sys_recv` already do. `xtask test-wait-exit-code`
deliberately waits *before* the child has run even once, to prove the
genuinely-blocking path rather than only the non-blocking
already-`Zombie` read.

**A killed or exited process with a parent becomes a `Zombie` regardless
of what state it was in when it ended** — including a `Suspended` child
killed before ever running, matching real Unix `kill()`+`wait()`
semantics (a process you kill yourself still needs reaping). This
surfaced as a real gap in the *test* for the leak fix: the first version
of `test-process-lifecycle`'s dummy process spawned and killed a child
in a loop without ever calling `SYS_WAIT`, and correctly ran out of
process-table slots on real `Zombie` accumulation well before its
target iteration count — not a kernel bug, a reminder that `SYS_KILL`
alone was never meant to fully release a child's slot on its own.

**When a process terminates, its own leftover children are handled
once**, inside the same primitive: an orphaned `Suspended` child (never
released, so nothing but its now-gone parent could ever have released
it) is killed outright, recursively — closing the gap
`docs/adr/0006`'s Consequences section named as accepted-for-now.
An orphaned `Zombie` child (its only possible reaper just vanished) is
reaped straight to `Empty`, since leaving it would make its slot a
*permanent* leak, worse than the bounded one a still-live parent
leaves. This directly revises one specific claim in `docs/adr/0006`
("once released, the parent relationship confers no further
authority") — `SYS_KILL`/`SYS_WAIT` both treat `parent` as authority
over a child's *entire* life, not only its `Suspended` window.

### `SYS_KILL`

Same structural-authority model as grant/start (`target.parent ==
Some(caller_pid)`), extended to cover any state, not only `Suspended`.
Since nothing is its own parent and there's exactly one running process
on this single core, a kill target can never be `sched.current` — no
scheduler switch is ever needed.

- **A `Ready` target** is removed from the ready queue by draining and
  rebuilding it (`tarnos_kcore::RingBuffer` has no remove-by-value) —
  bounded to `MAX_PROCESSES` pop/push cycles, done once, never on the
  hot preemption path.
- **A `Blocked` target** may be referenced as `Waiter::Process(pid)`
  inside some `ipc::Endpoint`'s queue, but *which* endpoint isn't
  tracked anywhere reachable from the scheduler. Rather than add new
  bookkeeping for it, the stale reference is left in place, to be
  safely no-op'd later by the generation check below — mirroring the
  `Waiter::None` precedent `docs/adr/0005` already established for the
  same shape of problem ("don't act on a reference to something that's
  moved on"). `xtask test-kill-boundary` exercises exactly this case: a
  child is granted a capability, released, yielded to until it
  genuinely blocks inside its own `sys_recv`, and only then killed.

### `Pid` gained a generation counter, packed into the same `u64`

`Pid` stays exactly `pub struct Pid(pub u64)` — every existing
register-packing call site needed zero changes — but gained
`index()`/`generation()` accessors splitting it into a 32-bit table
index (low) and 32-bit generation (high). The scheduler tracks a
generation counter per table slot, independent of whatever currently
occupies it, bumped on every `allocate_pid()`. `with_process`,
`wake_blocked_process`, and `start_child` all now check it via a shared
`occupied_mut` helper before acting — this is what makes the
`SYS_KILL`-a-`Blocked`-process case above safe: a stale
`Waiter::Process(pid)` can never alias whatever unrelated process later
reuses that table slot. Packing was chosen over widening to a second
register specifically because `sys_grant` already uses every available
argument register — a second pid word would need to spill into `r8` for
no benefit the packed encoding doesn't already give for free.

One consequence of packing worth naming: two already-shipped
Milestone 3 test fixtures (`spawn-boundary-test`'s `boundary_test_process`)
hardcoded a target `Pid` of raw `0` to name "the bystander process,
which isn't my child." Once `allocate_pid` started bumping a slot's
generation to 1 before ever handing out its first `Pid` there, a bare
`0` named generation 0 at index 0 — which no process is ever assigned —
so the check still returned `InvalidTarget`, but from a generation
mismatch rather than the parent check it was actually meant to prove.
Fixed by hardcoding the correctly-packed value instead; the new
`kill-boundary-test` fixture does the same from the start.

## Consequences

- A process's exit now genuinely returns what it used — process-table
  slot and physical memory alike — proven by `test-process-lifecycle`
  running 48 spawn+kill(+wait) cycles (three times `MAX_PROCESSES`)
  and asserting the free-frame count is bit-for-bit identical before
  and after.
- A parent can learn exactly how and with what result a child ended
  (`test-wait-exit-code`), and can forcibly stop a misbehaving or stuck
  child in any state (`test-kill-boundary`), closing the last two gaps
  `docs/adr/0006` left open.
- **Wait-for-any-child** (POSIX `wait(-1)`-style) is not implemented —
  only wait-on-one-named-child. Revisit if something (a future shell
  managing several children) actually needs it; the current permission
  model's "at most one waiter per target" simplification would need
  rework first.
- **No per-process memory accounting or quota** — this milestone fixes
  the *leak*, not fairness or limits between processes.
- `ExitStatus::Faulted`/`Killed` carry no further detail (which vector
  faulted, which process killed it) — cheap to extend later, not needed
  yet.
- Process-control authority (`SYS_KILL`/`SYS_WAIT`) remains permanent,
  structural, and non-transferable — the seL4-style revocable
  `KernelObjectRef::Process` capability `docs/adr/0006` already
  considered and deferred is still deferred.
- `terminate_slot` releases the scheduler lock before dropping a
  process's `Box` — correct and sufficient on this single core, since
  interrupts are already disabled for the whole syscall/fault path that
  reaches it. A future multi-core milestone must re-examine this: on
  multiple cores, a second core could observe the now-`Empty`/`Zombie`
  slot before the first core's frame-freeing walk actually finishes.
