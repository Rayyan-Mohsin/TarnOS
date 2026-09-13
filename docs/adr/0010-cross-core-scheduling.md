# 0010: Cross-Core Process Scheduling

## Status

Accepted. Implemented across `kernel/src/task/scheduler.rs`,
`kernel/src/arch/x86_64/{percpu,syscall,lapic,idt,context_switch,smp}.rs`,
`kernel/src/memory/virt.rs`, `kernel/src/sync.rs`, `kernel/src/main.rs`,
`kernel/src/milestone7_tests.rs`, and `xtask`.

## Context

Milestone 6 (`docs/adr/0009`) booted every CPU core and gave each one a
correct, isolated GDT/TSS/IDT-load and a minimal LAPIC, but deliberately
touched nothing in `task::scheduler.rs`: every additional core just
parked in `loop { hlt() }` forever, interrupt-responsive but never
running a process. This milestone is the deliberate follow-up: make
processes actually run on more than one core, safely, held to the same
adversarial-testing bar every prior milestone has held itself to.

Direct research against the code (rather than M6's own predictions,
which undersold two spots) found three real, newly-reachable hazards
once a second core exists: the syscall entry stub's first instructions
touch `rsp` before saving a single register, so the usual
`percpu::core_index()` `CPUID` lookup can't run there yet;
`terminate_process` could tear down an `AddressSpace` a *different*
core's CR3 still pointed at, if the target was genuinely `Running`
elsewhere; and an idle core's CR3 keeps pointing at whatever it last
ran, which could be torn down and recycled by another core while this
one's page-table walks still assume it's live. All three are addressed
below. Three scope decisions kept this milestone as narrow as it could
be while still being genuinely useful: no per-core timer (forced
preemption is a future milestone; a process on another core only ever
gives up its core voluntarily), one shared ready queue with migration
accepted rather than per-core queues, and a new per-core CPU register
used nowhere except the one spot that strictly needs it.

## Decision

### Per-core "current process," one shared ready queue

`Inner.current` becomes `[Option<Pid>; MAX_CORES]`, indexed by
`percpu::core_index()`, still fully protected by `SCHEDULER`'s existing
lock. `set_current` mirrors every write into a new lock-free
`percpu::PerCpuSlot.current: AtomicU64` too, so another core can poll
"who's running there" (the cross-core kill protocol below) without
contending `SCHEDULER`. The ready queue stays the single shared
`RingBuffer` every core pops from — no per-core queues, no
work-stealing — which is what lets a process resume on a different core
than it last ran on. That migration is what makes several of the bugs
below reachable at all; it's accepted deliberately rather than
engineered around, per the milestone's own scope decision.

### Idle cores: an interruptible wait, not a permanent halt

Every core's idle path funnels through `idle_loop_on_own_stack`: drain
the kernel task executor (needed on *every* iteration, not just when a
process was found — a task's own wake, e.g. the console server's
endpoint receiving a message, never touches `SCHEDULER.ready` directly
and needs a fresh poll to make progress), pop the ready queue and
`resume()` if non-empty, otherwise `park_until_woken`. `park_until_woken`
is the one place true lost-wakeup safety matters: it checks the queue,
marks itself idle, checks *again*, then `sti; hlt` as one instruction
pair via raw `asm!` — the standard idiom, since `sti`'s one-instruction
interrupt shadow guarantees `hlt` itself executes before a wake IPI
landing in that exact instant is serviced, rather than being silently
coalesced into nothing. `notify_idle_cores` sends a targeted
`RESCHEDULE_VECTOR` IPI (a new fixed vector, `0x42`) to every core it
finds marked idle whenever new work is pushed (`spawn`, `start_child`,
`wake_blocked_process(_locked)`) — deliberately every idle core rather
than picking one, since a core that loses the race just re-parks.

A blocked or exited process's kernel stack is fixed *per process slot*,
not per core, so a core sitting there waiting for more work would leave
a live C call chain on a stack a *different* core could start reusing
the moment that same process gets migrated and takes its next syscall.
`abandon_process_stack_and_idle` closes this with a raw `asm!` stack
switch onto this core's own dedicated idle stack before ever entering
the wait loop — the same reasoning `smp::ap_entry_trampoline` already
applies to a freshly-booted AP with no process stack to abandon in the
first place. `switch_to_next_or_halt` (used by both `SYS_EXIT`/fault-kill
and blocking syscalls) reserves the permanent, unwakeable
`permanent_halt` for "the entire process table is empty," and takes this
interruptible path otherwise.

### Cross-core `SYS_KILL`: synchronous eviction

`terminate_process` now checks the per-core `current` array for the
target: if it isn't running anywhere, this is the same immediate
finalize as always. If it's `Running` on a *different* core, finalizing
here immediately would free an `AddressSpace` that core's CR3 still
names — so instead this records an eviction request
(`PerCpuSlot.evict_request`), sends that core a targeted
`RESCHEDULE_VECTOR` IPI, and bounded-spins on the lock-free `current`
mirror until it clears. The target core's IPI handler
(`on_reschedule_ipi`) reuses `context_switch`'s existing dual ring0/ring3
entry-stub shape (needed here for the same reason the CPU fault stubs
need it: it may have to redirect control to a different process than
whichever this core was running when the IPI landed) — if the request
still names this core's current process, it abandons the frame (the
process is being killed, not preempted), clears `current`, and falls
into the same idle-or-pop-next-ready path any reschedule IPI shares.
This keeps `SYS_KILL`'s contract identical to the single-core case: by
the time the syscall returns, the target is unconditionally gone.

### Per-core syscall entry, without a new CPU register

The original plan called for `GS_BASE`/`swapgs`, since the entry stub's
first instructions touch `rsp` before a single register is saved —
before `percpu::core_index()`'s `CPUID` lookup (which clobbers exactly
those registers) is safe to call. Implementation found a cheaper
option: `LSTAR` (the MSR naming which stub `SYSCALL` jumps to) is
*already* per-core hardware state, the same way each core already gets
its own TSS. `syscall_entry_stub!` generates one literal copy of the
whole entry stub per possible core (`MAX_CORES = 8`, kept in sync by a
compile-time assert since `global_asm!` can't be generated in a loop),
each closed over its own dedicated pair of scratch statics; each core's
`LStar::write` points at its own copy during bring-up. Every other
"which core is this" lookup elsewhere in the kernel still uses the
existing `CPUID` scan, unchanged — this new mechanism is scoped to the
one spot that strictly needs it, per the milestone's own scope decision.

### Closing the idle-core stale-CR3 hazard

A single static, upper-half-only `AddressSpace` (`IDLE_ADDRESS_SPACE`)
that every core activates while idling with nothing to run. Since every
real process switch already calls `activate()` (an ordinary `mov cr3`,
which flushes the entire non-global TLB — nothing in this codebase uses
PCID or global pages), this closes the gap with no IPI-based shootdown
protocol needed. Built *eagerly*, at boot, before any test ever
snapshots `memory::phys::free_frame_count()` as a baseline — building it
lazily on first idle was tried first and produced a real, reproduced
false-positive "memory leak" (see below).

### `ipc::endpoint`'s lock-ordering: doc-only fix

Traced every caller: nothing ever calls `Endpoint::try_send/recv` while
holding `SCHEDULER`'s lock, and the existing drop-the-endpoint-lock-
before-`wake_receiver` discipline already defends against the same-object
re-locking hazard regardless of core count. Only the comments changed,
correcting "single-core self-deadlock" framing to describe the same-core
reentrancy hazard it actually is — mirroring the identical correction
M6 already made to `sync::SpinLock`'s own doc comment.

## Real bugs found (all via adversarial QEMU stress-testing, not review)

This milestone found more real concurrency bugs than any prior one —
expected, since it's the first time more than one core can genuinely
touch the same scheduler state at once. Each was found by a specific,
diagnosable test failure (a wrong exit status, a hang short of the
expected halt line, a fault at an address that moved between runs — the
signature of two cores racing on the same memory, not a deterministic
logic error), diagnosed with a temporary in-memory flight-recorder
(`(seq, tag, core, pid)` packed into an `AtomicU64` ring, dumped from
the panic handler or a periodic timer tick — avoiding both timing
perturbation and multi-core UART interleaving), then fully removed once
found.

**Found while first bringing processes up on more than one core:**

- `scheduler::start()` assumed popping the ready queue would always
  find `init` there — true with one core, false the instant an idle AP
  can race the BSP for the very same `spawn()` call. Fixed by making
  `start()` simply enter the same idle-scheduling path every core
  shares.
- `block_current_process` never cleared the blocking core's `current`
  entry. If that process was later woken and resumed on a *different*
  core, the original core's stale entry let the next timer tick
  overwrite the process's real, actively-executing trap frame with
  garbage and double-queue it — two cores running the same process and
  kernel stack concurrently.
- The stack-sharing hazard `abandon_process_stack_and_idle` (above)
  exists to close was first caught here: `percpu::slot()` observed being
  called with a stack address instead of a core index — a corrupted
  local, not a logic error, from a different core overwriting a stack
  this one hadn't finished using.
- `IDLE_ADDRESS_SPACE` built lazily looked exactly like a physical-memory
  leak to `test-process-lifecycle`'s own before/after frame-count check,
  since its one-time PML4 allocation could land at an unpredictable
  point mid-test. Fixed by building it eagerly at boot instead.
- A child left merely `Ready` (already woken, not yet actually resumed)
  at the exact instant its parent exited kept a stale `parent` pointing
  at a `Pid` nothing would ever reuse; its own later exit then produced
  a permanently unreapable `Zombie`, hanging `all_processes_empty`
  forever. Fixed by clearing `parent` on any non-`Suspended` orphan in
  `reap_children_of`, matching a real OS re-parenting an orphan.
- The idle loop only drained kernel tasks when it had already found a
  process to run; once genuinely idle-waiting, a task becoming ready
  afterward (e.g. completing a rendezvous with a `Blocked` process) was
  never picked up. Fixed by draining unconditionally on every iteration.

**Found writing this milestone's own dedicated test scenarios:**

- **The deepest bug of this milestone.** `wait_for_child` recorded the
  caller as a target's `wait_waiter` and returned a sentinel telling
  `sys_wait` to separately call `block_current_process` afterward — two
  distinct `SCHEDULER` lock acquisitions with a gap in between. A
  different core completing that wait (the target exiting) in exactly
  that gap would see `wait_waiter` already set and immediately try to
  wake and resume the caller — while the caller was still genuinely
  running right there and hadn't yet persisted *this* syscall's real
  trap frame anywhere. `test-smp-wait-cross-core` failed almost every
  run, with three different symptoms across repeated attempts (two
  different ring-0 general-protection-fault addresses, one silent
  hang) — the clearest possible evidence of a live data race rather
  than a deterministic bug. Fixed by folding the "still alive, must
  block" transition into `wait_for_child` itself, under the one lock
  acquisition that already found the target alive. `SYS_SEND`/`SYS_RECV`
  block through `ipc::Endpoint`'s own separate lock rather than this
  same pattern and were not observed to fail under any test in this
  milestone, but share the same *shape* of hazard in principle — flagged
  below as a tripwire for whichever future milestone adds a cross-core
  blocking-IPC test.
- Even after that fix, `switch_to_next_or_halt` still dropped
  `SCHEDULER`'s lock *before* abandoning a blocked/exiting process's
  stack, leaving a smaller but nonzero window for the same class of
  race — confirmed by the same test still failing on roughly a third of
  repeated runs. Closed completely (not just shrunk) by never dropping
  the lock at all in that path: the `SpinLockGuard` is left to
  `core::mem::forget`, which keeps the underlying lock held right up
  through the raw stack switch, and `idle_loop_trampoline` releases it
  with a new `SpinLock::force_unlock` only once safely on the idle
  stack. Every operation that could make the process visible to another
  core again — waking it, migrating it, reusing its table slot — needs
  that same lock, so holding it continuously makes the race structurally
  impossible rather than merely unlikely.
- `test-smp-kill-cross-core` itself immediately hit a third bug — in its
  own boot code, not the kernel: allocating both the target's and
  killer's `Pid`s up front, before spawning either, silently handed out
  the *same* table index twice (`allocate_pid` only reserves a `Pid`
  value; it's `spawn`/`spawn_suspended` that actually marks a slot
  non-`Empty`, exactly as `allocate_pid`'s own doc comment already
  warned callers to sequence around). Both processes ended up sharing
  one table slot under two different `Pid` generations, double-scheduled
  under real cross-core timing. Fixed by spawning the target immediately
  (parent fixed up afterward via the existing `with_process`, once the
  killer's `Pid` is actually known) rather than allocating both up front
  — the same allocate-then-spawn-immediately convention
  `kill-boundary-test`'s bystander process already used correctly.

## Testing

New `xtask` scenarios, each following the established feature-gated-
`main.rs`-block-plus-adversarial-and-happy-path pattern:

- `test-smp-sched-concurrency`: two real dummy processes, each
  free-spinning on `SYS_YIELD`, scheduled onto two different cores;
  boot code polls every core's `PerCpuSlot.current` directly for both
  pids appearing on different cores at the same instant.
- `test-smp-wait-cross-core`: reuses the existing single-core
  `wait-exit-code-test` feature unmodified, at `-smp 4` — the parent
  blocks in `SYS_WAIT` before its child has ever run, and the child
  (very likely picked up by a different, previously-idle core) exits
  and must correctly wake and report back across cores.
- `test-smp-kill-cross-core`: a killer yields a few times (letting its
  child actually start running, most likely on a different core) then
  `SYS_KILL`s it — the direct adversarial exercise of the cross-core
  eviction protocol — then spawns, starts, and waits on one more
  ordinary child to confirm the machine is still fully healthy right
  after the eviction (a missing idle-core stale-CR3 fix would surface as
  a spurious page fault exactly here).
- `test-smp-sched-stress`: reuses the existing `process-lifecycle-test`
  feature (rapid repeated spawn/kill/wait cycles) unmodified, at
  `-smp 4`, the target for the lost-wakeup and eviction-protocol hazards
  under sustained pressure.
- All 16 pre-existing scenarios re-verified unmodified, plus
  `test-smp-regression`'s existing spot-check that other cores merely
  existing nearby doesn't perturb single-core fault/IPC logic.
- All four new scenarios pass reliably across repeated stress runs
  (15-30 repeats each) after the fixes above, not just once.
- `cargo test -p tarnos-kcore -p tarnos-abi` and `cargo clippy` stay
  clean; GitHub Actions CI green on the pushed branch.

## Consequences

- Real processes now run concurrently across every booted core, with
  migration, cross-core blocking wake-up, and cross-core `SYS_KILL` all
  verified under repeated adversarial stress — not merely "didn't crash
  once."
- **`SYS_SEND`/`SYS_RECV`'s blocking path is a known, un-exercised
  tripwire.** `ipc::Endpoint::try_send`/`try_recv` register a process as
  a waiter under the endpoint's own lock, then `sys_send`/`sys_recv`
  separately call `block_current_process` — the identical two-phase
  shape `wait_for_child` had before this milestone's deepest fix above,
  just mediated through a different lock. No test in this milestone
  exercises a cross-core wake through this specific path (the existing
  `test-blocking-ipc`/`test-double-send` never had a second core able to
  race the blocking one), so it has not been observed to fail — but it
  has not been proven safe either. Closing it properly means extending
  atomicity across `Endpoint`'s lock and `SCHEDULER`'s, which needs its
  own careful lock-ordering audit; left for a follow-up milestone (or
  immediate hardening pass) with its own dedicated cross-core blocking-
  IPC adversarial test, rather than guessed at here.
- **Still no per-core timer.** A process on another core can only be
  interrupted by its own choice (yield, block, exit), a fairness limit
  rather than a safety one. Forced cross-core preemption stays deferred.
- **Still no TLB shootdown / per-core ready queues / work-stealing** —
  none needed yet, per the milestone's own scope decisions; the
  idle-CR3 fix's "every CR3 write flushes everything" assumption is a
  tripwire for whichever future milestone adopts PCID or global pages.
- **`MAX_CORES = 8`'s syscall-entry-stub duplication is hand-maintained.**
  `syscall_entry_stub!`'s eight copies are kept in sync with
  `percpu::MAX_CORES` by a compile-time assertion, not generated from
  it — `global_asm!` can't be produced in a loop. Raising `MAX_CORES`
  needs a matching new copy added by hand.
- CPU hotplug, NUMA/topology awareness, real-time or priority
  scheduling, and explicit CPU-affinity syscalls remain out of scope,
  as do keyboard drivers, filesystems, and the rest of the longer-term
  roadmap.
