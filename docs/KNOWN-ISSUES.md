# Known Issues

## The cross-core scheduling corruption (open)

**Status:** open, under active investigation since Milestone 9. Fails
safely. Contained to one adversarial test feature that is deliberately
excluded from normal builds and CI.

### What it is

Under sustained, heavy scheduling activity across more than one CPU
core, some code path in the kernel's scheduler/dispatch machinery
occasionally corrupts a small value — a register, a stack-resident
local, a heap-allocated struct field — with a plausible-looking but
wrong value (a stray small integer, a real kernel address with one
byte altered, a legitimate value from an unrelated code path landing
in the wrong location). The corrupted value is eventually read back
(as an instruction pointer, an array index, a stack pointer, ...) and
the kernel panics — a hardware fault or a Rust-level bounds/pointer
check, never a silent continuation. **The root cause has not been
found.** Twenty ADRs (`docs/adr/0012` through `docs/adr/0029`) document
the investigation in full: what's been tried, what's been ruled out
with direct evidence, and what each round's own captures showed.

### When it applies

- **Requires multiple cores.** Every `-smp 1` run across this entire
  investigation — 20+ runs, across every ADR — has passed. A
  single-core build or boot is unaffected by everything measured so
  far (confirmed directly in `docs/adr/0017`, not merely assumed).
- **Requires sustained, heavy cross-core scheduling activity to
  reproduce reliably.** The measured failure rate is roughly 35-90%
  under `test-kitchen-sink` (an adversarial stress scenario — several
  processes concurrently driving IPC, heap growth, process lifecycle,
  and cross-core `SYS_KILL`, plus background scheduling pressure, all
  under `-smp 4`), varying by exact QEMU acceleration settings
  (`docs/adr/0017`'s own scaling table: 0% at `-smp 1`, ~1% at `-smp 2`,
  50-90% at `-smp 4` depending on TCG threading mode). It has also been
  reproduced, at a similar rate, by a *much* smaller workload — 8
  processes doing nothing but `SYS_YIELD` in a tight loop forever, no
  process creation or destruction at all (`docs/adr/0019`) — so it is
  not specific to any one syscall or subsystem; it lives somewhere in
  the shared preemption/dispatch path every process goes through
  (`on_timer_tick`/`on_syscall_yield` → `switch_to` →
  `context_switch::resume`).
  **This is not something that fires with the same certainty on
  ordinary, light multi-core use** (a couple of cooperating processes,
  the normal boot sequence, any of the 26 scenarios in `test-all`
  — 22 as of Milestone 10, joined by Milestone 11's four virtio-blk
  scenarios, none of which touch scheduling/dispatch at all) —
  every one of those passes reliably and is run in CI on every push.
  The failure rate scales with how much concurrent scheduling pressure
  is actually applied.
- **Fails safely.** Every observed failure is a kernel panic into a
  controlled halt (`broadcast_panic_halt` stops every other core
  immediately, so nothing else can also observe or extend the
  corruption) — never silent corruption that keeps the machine running
  with bad state. This has been true since `docs/adr/0012` fixed two
  real deadlock-on-panic bugs specifically so that every future
  occurrence of this corruption would report itself instead of
  hanging.

### Why `test-kitchen-sink` is not in `test-all`/CI

`test-kitchen-sink` (`cargo run -p xtask -- test-kitchen-sink`) *is*
the reproduction for this bug, not a real workload — it is deliberately
adversarial, combining every subsystem's stress scenario at once
specifically to make this race as likely as possible to fire. Wiring
a scenario with a 35-90% failure rate into `test-all`/CI would make
every push look broken regardless of whether it actually changed
anything, so it is kept as its own standalone command instead
(documented in `xtask/src/main.rs`'s own usage text and
`test_kitchen_sink`'s doc comment) — runnable directly by anyone
continuing this investigation, without being part of the suite that
gates a normal change.

### Where the investigation currently stands

`docs/adr/0029` is the most recent round. Four rounds of guard-paging
every major piece of scheduler/process state (`docs/adr/0025`-`0027`)
have each come back clean — ruling out a wild write landing *outside*
any of those specific regions — while direct captures have shown the
corruption reaching heap-allocated process state, a stack-resident
`SpinLockGuard`, and a mathematically-bounded local inside the panic
diagnostic path itself. The converging picture (per ADR 0027's own
Consequences) is a wild write or corrupted pointer touching stack and
heap memory alike from somewhere in the interrupt-entry/context-switch
path's own register and stack-frame handling, rather than a defect
specific to any one data structure's own logic — guard-paging more
individual structures is judged unlikely to be productive from here.
ADR 0028 restored a working live-debugging setup
(`-accel tcg,thread=single` fixes a QEMU/GDB multi-vCPU incompatibility
that had blocked live capture since ADR 0022) and caught a corrupted
code pointer recurring at the same value across two separate captures
(`LAPIC_MMIO_VBASE`/`REG_EOI`); ADR 0029's own follow-up batch didn't
reproduce that exact value again, so it's a real but not dominant
signature, not a reliable target to watch for specifically.

**The next concrete step** (per ADR 0029's own Consequences): any
future live-debugging session should set a hardware watchpoint on
whatever a *fresh* capture's own evidence points at, decided at the
time of that capture, rather than pre-committing to any one
previously-seen value or address — and should look at the
interrupt-entry/context-switch path's own register and stack-frame
handling directly, per ADR 0027's redirection, rather than at any
further individual data structure.

### What a future contributor needs to know

- Building the filesystem, a real driver, or anything else on top of
  the process/scheduler/IPC core does not need to wait for this to be
  fixed — every normal code path (`test-all`'s 26 scenarios, the whole
  of Milestone 10's own audit, and Milestone 11's virtio-blk driver and
  its own four scenarios) is unaffected. This bug requires
  `test-kitchen-sink`'s own specific, deliberately adversarial stress
  shape to reproduce at any practical rate.
- If a future scenario or real workload starts hitting this under
  *normal* (non-adversarial) use, that is new, important evidence this
  investigation has not yet seen — the failure rate has, so far, always
  scaled with deliberately applied scheduling pressure, not appeared
  under light, ordinary use.
- Continuing the investigation itself is a distinct thread of work,
  not part of Milestone 10 or whatever comes after it (filesystem/
  driver milestones) — see `docs/MILESTONE-10-LAST-BASE-LEVEL-CHECK.md`'s
  own Non-goals. Pick it up by reading `docs/adr/0029` (the most recent
  round) backward as far as needed for context, not by re-deriving
  this investigation's reasoning from scratch.
