# 0011: Hardening the Core — LAPIC Timer, Forced Preemption, and Two New Cross-Core Races

## Status

Accepted. Implemented across `kernel/src/driver/uart.rs`,
`kernel/src/earlycon.rs`, `kernel/src/lang_items.rs`,
`kernel/src/task/{process,scheduler}.rs`,
`kernel/src/arch/x86_64/{lapic,context_switch,idt,mod,smp,syscall}.rs`,
`kernel/src/memory/{virt,phys}.rs`,
`kernel/src/milestone8_tests.rs`, `libs/tarnos-kcore`, `libs/tarnos-abi`,
and `xtask`.

## Context

ADR 0010 shipped real cross-core scheduling but deliberately left two
things out of scope: forced preemption (a process on a non-BSP core
could only ever give up its core voluntarily — the legacy 8259 PIC/PIT
can only route an interrupt to one core) and any property-based/fuzz
testing anywhere in the workspace. It also flagged one specific,
un-exercised hazard of its own: `SYS_SEND`/`SYS_RECV`'s blocking path
has the identical two-phase check-then-block shape the `wait_for_child`
race had, just mediated through `ipc::Endpoint`'s lock instead of
`SCHEDULER`'s. Before moving on to keyboard drivers, filesystems, or
anything else "higher-level," this milestone exists to close all three
gaps and hold the core to substantially more test rigor than the
project has had so far — the UART's 16550 driver also had a
long-standing gap of its own (LSR overrun/framing/parity error bits
were never checked, harmless under QEMU but a real gap on real
hardware).

While closing those gaps and building the property tests, prototyping a
combined multi-workload stress scenario (several processes each driving
a different subsystem concurrently, plus background scheduling
pressure) surfaced two further real bugs — described below. Getting
that combined scenario itself fully reliable surfaced a third, deeper
cross-core issue that resisted the same level of investigation this
milestone's other races got; rather than block this milestone
indefinitely chasing it, the scenario itself and that remaining issue
are deferred to Milestone 9, and this milestone ships everything that
*is* solid: the LAPIC timer, the `pending_wake` fix, the UART hardening,
the property tests, and the two newly-found-and-fixed bugs below, all
independently verified.

## Decision

### UART LSR error-bit handling

`driver::uart::Uart16550::try_read_byte` now checks the overrun/framing/
parity bits (LSR bits 1–3), not just `LSR_DATA_READY`. On an error bit,
the data register is still drained (real hardware requires reading it
to clear the error latch) but the byte is discarded rather than
returned as valid data, and a lock-free `UART_LSR_ERROR_COUNT: AtomicU64`
is bumped instead of logging inline — logging from inside this path
would later turn into a lock-ordering hazard (see below).

### `SYS_SEND`/`SYS_RECV`'s cross-core race: deferred-wake

`Process` gains `pending_wake: Option<WakeResult>`, mirroring
`wait_waiter`'s "at most one legitimate claimant" reasoning. If
`wake_blocked_process_locked` finds its target still `Running` (it
registered as an `ipc::Endpoint` waiter but hasn't reached its own
`block_current_process` call yet — the exact race window), it stashes
the wake result there instead of applying it immediately.
`block_current_process_locked` checks and consumes it the instant it
finishes the transition to `Blocked`, under the same `SCHEDULER`
acquisition. Chosen over the two more invasive designs considered (
nesting `SCHEDULER` and `Endpoint::slot` in a fixed order, either
direction) because it needs no new permanent cross-module
lock-ordering rule — `ipc::endpoint.rs` and `syscall.rs` are
unchanged.

### Per-core LAPIC timer and forced preemption

New hand-rolled register constants in `arch::x86_64::lapic` (the
`x86_64` crate has no typed APIC-timer support) and a new
`LAPIC_TIMER_VECTOR`. The BSP calibrates a periodic reload value against
the already-running PIT once, at boot (briefly re-enabling interrupts
during the calibration window, since it reads `interrupts::ticks()`,
which only advances via the PIT's own handler); every core — BSP and
each AP — then arms its own timer from that shared calibrated value.
The new entry stub mirrors `reschedule_entry`'s dual ring0/ring3-
dispatch shape and reuses `task::scheduler::on_timer_tick` completely
unmodified (already generic over "whichever core calls it"), EOI'd via
the LAPIC directly. Deliberately does *not* call the PIT's own
`on_timer_tick_bookkeeping()` — that function's `TICKS` counter is
trusted elsewhere as "the BSP's own PIT-tick count"; every core's LAPIC
timer also bumping it would silently redefine it as a cross-core sum.

### Cross-core `SYS_KILL`: migration-aware retry

Forced preemption means a `SYS_KILL` target can now migrate to a
different core in the middle of an in-flight eviction — a case the
single-shot eviction protocol from ADR 0010 didn't account for.
`terminate_process` is now a loop: each iteration re-scans `current`
for the target under a fresh `SCHEDULER` acquisition; if it isn't
running anywhere, the check-and-finalize happens in that same
acquisition (closing the same TOCTOU shape ADR 0010's deepest fix
already established the pattern for); otherwise it IPIs the owning core
and bounded-spins (`TIMEOUT_SPINS`, deliberately small — see below) on
the lock-free `current` mirror before looping back to re-scan.

### Physical-frame double-free hardening

`memory::phys::BitmapFrameAllocator` gains a second bitmap, `usable`,
set once at `populate()` time and never modified again, alongside the
existing `bitmap` (which flips a frame between free/allocated). The
existing double-free `debug_assert!` (`!bitmap.is_free(idx)`) can't
distinguish "already allocated, now double-freed" from "never in the
usable pool at all" — both read `is_free == false`. `deallocate_frame`
now also asserts `usable.is_free(idx)`, turning a silent, first-time
"free" of a frame this allocator never owned (kernel `.text`, reserved
memory, a corrupted page-table entry) into an immediate, attributable
panic instead of quietly handing that frame back out later.

### `sys_sbrk`: two short critical sections instead of one long one

`sys_sbrk`'s frame-allocation-and-mapping loop no longer runs inside
`with_current_process`/`SCHEDULER`. It now validates and reserves the
grow (short critical section), releases the lock, does the actual
per-page `allocate_frame`/`map` work using a new `memory::virt::map_in`
free function (the same mapping `AddressSpace::map` does, reachable
from just a `pml4_frame: PhysFrame` — `Copy`, cheap to carry across the
gap — rather than a live `&AddressSpace`), then re-acquires the lock
only to commit `heap_end`. Sound because only the calling process's own
single execution thread ever touches its own `heap_end` or extends its
own `AddressSpace` — nothing else can race the gap between the two
acquisitions, and a concurrent `SYS_KILL` targeting this same process
still correctly waits for the syscall to finish first (see below).

### `allocate_pid`: atomic slot reservation

`allocate_pid` now marks its chosen slot `Slot::Reserved` in the same
`SCHEDULER` acquisition that finds it, instead of leaving it `Empty`
until the caller's later `spawn`/`spawn_suspended` call fills it. A
caller that fails before ever reaching that call (today: only
`SYS_SPAWN`'s handler, on ELF-load failure) must call the new
`release_reservation` to give the slot back rather than leaking it
permanently.

## Real bugs found (all via adversarial QEMU stress-testing, not review)

- **Panic-handler reentrancy deadlock.** A second fault landing on the
  same core while the panic handler was still mid-print (holding
  `earlycon`'s lock on its own call stack) re-entered the handler and
  deadlocked trying to re-lock it — observed as a hang with a truncated
  panic line instead of a second, distinct one. Fixed with a permanent
  per-core reentrancy guard in `lang_items::panic`: a second fault on
  the same core halts immediately instead of re-entering.
- **`TIMEOUT_SPINS` far too large under TCG emulation.** The cross-core
  eviction wait's original bound (100,000,000) meant hitting it even
  once in the new migration-aware retry loop could burn most of a test
  scenario's wall-clock budget — observed as an intermittent "process
  never reported a result" hang. Reduced to 2,000,000 (safe regardless
  of how short this bound is, since the actual correctness guarantee
  comes from the outer loop's atomic check-and-finalize, not from this
  wait completing) and the core's own `evict_request` is now cleared on
  timeout, so a late-arriving IPI for an abandoned request can't be
  misread as still relevant to whatever that core is asked to evict
  next.
- **A ~1-in-40-run residual `test-smp-kill-cross-core` failure,
  investigated but not fully root-caused.** Even after the two fixes
  above, a rarer variant of the same class of race kept surfacing under
  sustained stress. Diagnosed with a temporary in-memory flight
  recorder (`(seq, tag, core, pid)` packed into an `AtomicU64` ring,
  dumped from the panic handler — the same technique ADR 0010's own
  deepest bug was found with), which pointed the remaining failure to a
  fault occurring inside panic-formatting machinery itself under
  extreme forced-preemption pressure, not the eviction protocol proper.
  The flight recorder was removed once it had done its job (per this
  project's own established convention); the panic reentrancy guard it
  led to was kept permanently. This residual case is accepted as a
  documented, rare risk rather than chased further — the same call ADR
  0010 never had to make, since nothing in that milestone got this
  close to fully closing its own hazard before diminishing returns set
  in.
- **`forced_preempt_busy_process` used a plain Rust `loop { spin_loop() }`**
  instead of raw inline asm, violating `Process::new_dummy`'s own
  documented constraint. In an unoptimized debug build the call didn't
  inline, producing an out-of-page call whose PC-relative target was
  wrong once the code was copied to its new base address — an
  instruction-fetch page fault. Fixed with a raw `asm!` loop.
- **Preemption-detection blind to redispatch-to-self.** The original
  `test-smp-forced-preempt` check watched `PerCpuSlot.current` for a
  value change — blind to the actual scenario, since with nothing else
  ever ready on the busy process's own core, a preempted process is
  immediately redispatched right back to itself and `current` never
  changes even though real preemption keeps happening. Fixed with a
  dedicated `preempt_count` counter, bumped unconditionally in
  `on_timer_tick` whenever a running process is preempted regardless of
  what gets dispatched next.
- **`earlycon`/`driver::uart` dual-writer race on the physical COM1
  port.** `earlycon`'s bare port writes (`earlyprintln!`/panics/boot
  messages) and `driver::uart::Uart16550`'s real, LSR-checked writes
  both target the same physical transmit register but were protected by
  two entirely separate locks — each correctly serializing writers
  *within* its own path, doing nothing to stop the two paths
  interleaving with each other. Reproduced as a visibly garbled log
  line (two lines byte-interleaved) under `test-smp-forced-preempt`.
  Fixed by unifying both paths under one shared, `pub(crate)`
  `earlycon::COM1_TX_LOCK`, acquired before each path's own inner lock;
  a new `write_line` helper holds it across both a message and its
  trailing `"\r\n"` in one acquisition. This introduced its own
  lock-ordering hazard — `try_read_byte`'s LSR-error branch had been
  calling `earlyprintln!` while `COM1`'s own lock was already held, the
  opposite order from `write_bytes`/`write_line` — closed by replacing
  that log call with the lock-free `UART_LSR_ERROR_COUNT` counter
  mentioned above.
- **`SyscallError::from_retval` overflow at `i64::MIN`.** Writing
  `tarnos-abi`'s new property tests (below) surfaced a real, if
  never-triggered-by-a-real-kernel, bug: `(-retval) as u64` panics under
  debug overflow-checks for `retval == i64::MIN`, which has no positive
  `i64` representation to negate into. Fixed with
  `retval.unsigned_abs()`, plus a deterministic regression test pinning
  this exact boundary value (property-test sampling alone isn't
  guaranteed to hit it every run).
- **Cross-core `allocate_pid` slot-reservation race.** Found while
  prototyping the combined multi-workload stress scenario deferred to
  Milestone 9 — the first time `SYS_SPAWN` could be a process's very
  first instruction after being scheduled, letting a second core's
  `allocate_pid()` land in the gap between a first core's own
  `allocate_pid()` and its matching `spawn`. Both calls would see the
  same slot as `Empty` and be handed the identical table index (with
  different generations); whichever `spawn`/`spawn_suspended` ran
  second silently overwrote the other's `Process` in the table, while
  the loser's own `Pid` kept resolving to the winner's process —
  aliasing two unrelated processes. Observed in practice as a process
  running another process's code, from process-index confusion alone,
  no unsafe code or memory corruption involved in the bug itself. Fixed
  by making the slot reservation atomic (see Decision, above).
- **`sys_sbrk` holding `SCHEDULER` across its whole frame-allocation
  loop.** Also found via the same prototyping. Never visibly wrong on
  any single-workload heap-growth test, but once heap growth ran
  concurrently with other cores under constant forced preemption, every
  other core's `on_timer_tick`/`on_syscall_yield`/`wake_blocked_process`
  — all needing the same lock — stalled behind it for the entire grow;
  with more than one such lock-holding stretch overlapping across
  cores, the resulting head-of-line blocking was severe enough to look
  like a hang. Fixed by splitting the critical section (see Decision,
  above).

## Testing

- New proptest-based property tests in `libs/tarnos-kcore` (`RingBuffer`
  against a capacity-bounded `VecDeque` model, `Bitmap`'s
  `allocate()`/`free_count()` invariants, `CapTable`'s
  lookup-matches-most-recent-insert invariant, `ipc::Slot`'s full
  three-state rendezvous machine against a reference model) and
  `libs/tarnos-abi` (previously zero tests: round-trip and
  never-panics properties for `pack_program_name`/`unpack_program_name`,
  `Message::from_str_lossy`/`as_str_lossy`,
  `SyscallError::as_retval`/`from_retval`, `ExitStatus::to_regs`/
  `from_regs`).
- New `kernel/src/milestone8_tests.rs` + `xtask` scenarios:
  - `test-smp-send-cross-core`: a receiver blocks in `SYS_RECV` on a
    fresh endpoint before anything has been sent; a sender, spawned
    right after and very likely landing on a different, previously-idle
    core, immediately `SYS_SEND`s — directly exercising the
    `pending_wake` fix.
  - `test-smp-forced-preempt`: a process busy-loops forever making
    *zero* syscalls, so only the new per-core LAPIC timer can preempt
    it; confirms genuine preemption (via `preempt_count`, not `current`),
    that an ordinary process still runs alongside it, and that
    `SYS_KILL`'s cross-core eviction protocol still works against a
    target that was forcibly, not cooperatively, scheduled.
  - Both pass reliably across repeated stress runs (15–30 repeats).
- All ~23 pre-existing scenarios re-verified unmodified, run in full
  twice, in isolation (a documented lesson from this milestone: running
  multiple QEMU-heavy background jobs concurrently causes spurious,
  host-CPU-contention-driven timeouts on otherwise-passing single-core
  scenarios — always verify suspicious failures with an isolated rerun
  before treating them as regressions).
- `cargo test -p tarnos-kcore -p tarnos-abi` and
  `cargo clippy -p tarnos-kcore -p tarnos-abi --all-targets -- -D warnings`
  stay clean; GitHub Actions CI green on the pushed branch.

## Consequences

- Every core now gets forced preemption, not just the BSP; a process
  making zero syscalls can no longer monopolize a core forever.
- `SYS_SEND`/`SYS_RECV`'s cross-core blocking path — ADR 0010's own
  flagged tripwire — is now closed and directly tested, the same way
  `wait_for_child` was closed in that milestone.
- **A rare (~1-in-40-run) residual failure in
  `test-smp-kill-cross-core` remains, documented rather than
  eliminated.** Real, substantive fixes (the migration-aware retry
  loop, the atomic check-and-finalize, the timeout reduction) measurably
  improved the failure rate; the exact remaining root cause was not
  found despite flight-recorder-based investigation pointing at
  panic-formatting machinery under extreme stress. Revisit if it starts
  reproducing more often, or if a future milestone's own stress testing
  sheds more light on it.
- **The combined multi-workload stress scenario is deferred to
  Milestone 9.** Prototyping it found and fixed two more real bugs
  (above), which are landing in this milestone regardless of the
  scenario itself, but getting the scenario reliably green surfaced a
  further, deeper cross-core issue that resisted the same level of
  investigation this milestone's other races got. Rather than block
  everything above behind that one open question, it and the scenario
  itself move to their own milestone.
- Physical-frame accounting is now harder to get silently wrong: a
  stray `deallocate_frame` on a frame this allocator never owned is now
  a loud, attributable panic instead of quiet corruption.
- Keyboard drivers, filesystems, and the rest of the longer-term roadmap
  remain explicitly what this milestone (and the next) exist to
  precede, not begin.
