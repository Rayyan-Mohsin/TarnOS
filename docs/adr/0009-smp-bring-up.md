# 0009: SMP Bring-Up

## Status

Accepted. Implemented across `kernel/src/arch/x86_64/{smp,lapic,percpu,gdt,idt,mod}.rs`,
`kernel/src/{earlycon,main}.rs`, `kernel/src/memory/phys.rs`, and `xtask`.

## Context

Milestones 1-5 hardened process isolation, dynamic process creation,
lifecycle management, and userland dynamic memory — all on a kernel that
had only ever run on one CPU core. Three research passes confirmed
exactly what that meant in practice: the frame allocator and boot page
mapper were already safe for concurrent access, but `task::scheduler`'s
`sched.current` was a single global scalar, the GDT/TSS was one global
structure whose RSP0/IST fields are inherently per-core hardware state,
the syscall entry path's stack-pointer globals were bare cells two
concurrent `SYSCALL` entries would corrupt, and there was no LAPIC/IOAPIC
code anywhere — only the legacy PIC/PIT, which can only ever route an
interrupt to one core. `sync::PerCpu<T>` existed only as a fully unused
placeholder. The `limine` crate (pinned at 0.6.5) already exposed an
`MpRequest`/`MpInfo` API for starting additional cores, and the RSDP had
been captured back in the very first milestone specifically so this
migration wouldn't need a boot-protocol change.

Given how much full SMP touches — AP boot, per-core GDT/TSS/IST, a new
LAPIC driver, per-core scheduler state, TLB shootdown, and a cross-core
lock-ordering re-audit — this was split into two milestones. **This one
is infrastructure only**: boot every core QEMU reports, give each one a
correct, isolated per-core GDT/TSS/IDT-load, and a minimal LAPIC (enable
+ EOI + targeted IPI, no periodic timer), and prove via QEMU-based xtask
tests that the additional cores are genuinely independent, executing
CPUs. **`task::scheduler.rs` is untouched** — `sched.current` stays
exactly the single global scalar it is today, so all 12 pre-existing
xtask scenarios are structurally guaranteed to keep passing unmodified.
A follow-up milestone will build actual cross-core process scheduling on
top of this foundation.

Two design forks were resolved in favor of the simpler option, both
explicitly to minimize new low-level surface in an already-large
milestone: per-core identity is a cheap `CPUID` read of a core's own
local-APIC ID looked up in a small fixed table, not a dedicated per-core
CPU register (`GS_BASE`); and no core gets a repeating timer this
milestone — each one parks in a low-power interruptible idle loop,
waking only on an explicit IPI.

## Decision

### Boot request and per-core identity

`kernel/src/main.rs` gained `static MP_REQUEST: MpRequest = MpRequest::new(0)`
(no x2APIC flag — plain MMIO xAPIC, the simpler option). `pub const
MAX_CORES: usize = 8` mirrors `MAX_PROCESSES`'s "generous fixed ceiling,
log-don't-panic on overflow" convention.

New `arch::x86_64::percpu`: a fixed `[PerCpuSlot; MAX_CORES]` table
(`lapic_id`, `ready`, `ipi_count`, `spin_count`, all atomic), filled in
once by the BSP during bring-up. `core_index()` reads the calling core's
own local APIC ID via `CPUID` leaf 1 (EBX bits 31:24) and linear-scans
the table for a match — cheap at `MAX_CORES = 8`. This directly replaces
`sync::PerCpu<T>`, which is deleted outright rather than kept alongside
a concrete replacement; `SpinLock`'s module doc comment is also
corrected, since its stated justification ("no second core to make
progress") was specific to same-core interrupt reentrancy, not a claim
that ceases to hold once a second core exists — the mechanism itself
needed no change, only the explanation.

### GDT/TSS: one shared GDT, one TSS descriptor + double-fault stack per core

`gdt::TSS_TABLE: [TssCell; MAX_CORES]` replaces the single `TSS`, each
with its own double-fault stack and IST slot 0 (striding
`DOUBLE_FAULT_STACK_BASE` by core index, eagerly mapped for every
possible core before any AP starts — the same discipline
`task::process::init_kernel_stacks` already established). `gdt::init_bsp`
builds one shared `GlobalDescriptorTable` — every core's CS/SS/DS/ES
selectors stay numerically identical; only the `ltr`-loaded TSS selector
differs per core. `gdt::init_ap(core_index)` does the per-core work: no
GDT rebuild, just loading the shared table into *this* core's own GDTR
and `ltr`-ing this core's own TSS selector (see the real bug below for
why the first part is load-bearing, not optional).

### Minimal LAPIC driver, no timer

New `arch::x86_64::lapic`: enable + spurious-vector register, `eoi()`,
and `send_ipi(target_lapic_id, vector)` via a two-word ICR write — no
INIT-SIPI-SIPI state machine is needed since `MpInfo::bootstrap()`
already handles AP startup. One new fixed IDT vector (`0x41`) for a test
IPI. No periodic timer: every core's idle state is `loop { hlt() }`
(interrupts enabled), low-power and fully interrupt-responsive. The
legacy PIC/PIT is untouched and keeps driving the BSP's real scheduler
timer exactly as before — this is a second, independent, parallel
interrupt path used only for the AP concurrency proof.

### AP entry: assume nothing about the landing state, verify it first

`limine` documents `MpInfo::bootstrap()`'s handshake mechanics but not
what CPU/paging/GDT/stack state an AP is in when it arrives. New
`arch::x86_64::smp::ap_entry_trampoline` (`global_asm!`) does the least
possible on whatever stack it's handed: read the core index out of
`MpInfo` by its known field offset, bounds-check it, compute that core's
pre-mapped idle-stack top by pure arithmetic, and jump there — no
Rust-level calls before that switch. Only once safely on a stack this
kernel mapped itself does `ap_entry_on_own_stack` log what it actually
inherited (CR3, RFLAGS.IF) before doing anything else, turning the
crate's documentation gap into an empirical, logged check rather than an
assumption. `smp::bring_up_aps` orchestrates it: assign every non-BSP
`MpInfo` a compacted slot, `bootstrap()` it, then wait — with a bounded,
logged timeout, never forever — for each to report ready.

### Two hardening drive-bys

`earlycon.rs`'s raw port writes had zero locking, justified only by
"single-threaded use" — now wrapped in a `sync::SpinLock` held across
one whole formatted line (message *and* its trailing `"\r\n"`, via a
dedicated `_println` rather than two separately-locked writes), so two
cores logging concurrently can't interleave mid-line. `memory::phys`'s
frame-allocator lock was a plain `spin::Mutex` with no documented
single-core-safety rationale anywhere, unlike every other lock in the
codebase — switched to `sync::SpinLock` as a zero-behavior-change
hardening pass while this exact class of gap was already being
addressed elsewhere in the same milestone.

### Two real bugs found by this milestone's own tests

Both were caught by `xtask test-smp-boot` failing exactly the way a
real bug should — not a vague hang, a specific, diagnosable symptom —
and both are exactly the kind of thing the "verify empirically" design
choices above exist to catch:

- **The GDT panicked ("GDT requires two free spaces to hold a
  SystemSegment") after only two of eight TSS descriptors were
  appended.** A TSS descriptor is a 16-byte "system segment" occupying
  *two* GDT entries, not one — `GlobalDescriptorTable`'s default
  capacity (8) has room for the four fixed segments plus only two such
  descriptors. Fixed by sizing the GDT explicitly:
  `1 (null) + 4 (segments) + 2 * MAX_CORES`.
- **Every AP silently hung with zero "ready" lines — a triple fault,
  not a panic, since it happened before that core had a valid IDT.**
  `gdt::init_ap` reloaded segment registers (`CS::set_reg`, which does a
  far-return through the GDT) without first pointing that core's own
  GDTR at the shared table — each core has its *own* GDTR even though
  the table contents are shared, so the AP was still running under
  Limine's temporary GDT, faulted on a selector that didn't exist in
  *that* table, and had no IDT yet to catch it. Fixed by having
  `gdt::init_ap` call the shared table's `.load()` (`lgdt`) first, and
  by reordering `smp`'s bring-up sequence to load the IDT *before*
  touching GDT/TSS state at all, so any future fault in that sequence
  produces a diagnostic instead of a silent reset.
- **A third, non-fatal but confirmed-wrong assumption**: `lapic.rs`'s
  first version reached the LAPIC's MMIO registers via
  `memory::virt::phys_to_virt` (the HHDM). Limine's HHDM only promises
  to cover installed RAM, not arbitrary device MMIO holes — the LAPIC's
  physical base is exactly such a hole, and touching it through HHDM
  page-faulted immediately. Fixed by explicitly mapping the LAPIC's
  actual physical page (read from `IA32_APIC_BASE`, never hardcoded) at
  a dedicated fixed virtual address, the same "confirmed directly, not
  assumed" bar this codebase already holds itself to elsewhere.

### Testing: proving independence without a timer

`xtask` gained an explicit `-smp N` parameter on every scenario (every
pre-existing one keeps passing `1` — the cheapest possible regression
check that nothing here perturbs single-core behavior). New scenarios:
`test-smp-boot` (`-smp 4`: every reported core reaches ready, and — only
under the `smp-boot-test` feature, where each AP free-spins a counter
instead of immediately parking — every one of those counters advances
by a comparable order of magnitude after a fixed delay, real evidence of
concurrent execution with no timer interrupt needed at all);
`test-smp-degraded` (the same kernel image at `-smp 2`, proving the
logic isn't hardcoded to a specific count); `test-smp-ipi` (a targeted
IPI reaches exactly one core and no other, and a send to a nonexistent
LAPIC ID doesn't hang or fault); and a regression spot-check re-running
`test-fault-isolation`/`test-blocking-ipc` at `-smp 4` to prove other
cores merely existing and idling nearby doesn't perturb the untouched
BSP-only scheduler/IPC/fault logic.

## Consequences

- Every CPU core QEMU/Limine reports now boots, gets a correct isolated
  GDT/TSS/IDT/LAPIC, and reaches a low-power interrupt-driven idle
  state — proven by `test-smp-boot`'s independent spin-counter evidence,
  not just "didn't crash."
- A specific core can be addressed directly via IPI (`test-smp-ipi`),
  the primitive a future milestone's TLB shootdown and cross-core wake-up
  will build on.
- **No process ever runs anywhere but the BSP.** `task::scheduler.rs`,
  `task::process.rs`, and `arch::x86_64::syscall.rs` are untouched;
  `sched.current` remains a single global scalar. This is deliberate,
  not an oversight — see the Context section's scope-split rationale.
- **No per-core timer.** Real cross-core preemption needs one; deferred
  to the milestone that actually schedules work across cores, since it
  isn't needed for anything this milestone proves.
- **No TLB shootdown.** No code path in this milestone ever unmaps a
  page another core could have cached — every core shares only the
  never-remapped kernel upper half — so the gap `docs/adr/0007` already
  flagged (`terminate_slot` observed mid-teardown from another core)
  remains exactly as open as before, unreachable until processes can run
  on more than one core.
- **No cross-core lock-ordering re-audit** of `ipc::endpoint`'s existing
  drop-then-recurse discipline — this milestone introduces no new
  cross-core lock interaction beyond ones already proven safe (the frame
  allocator, the boot page mapper).
- **`MAX_CORES = 8` is a fixed compile-time ceiling**, not a real
  hardware-topology query (no ACPI/MADT parsing — `MpRequest` supplied
  everything needed without it). A machine reporting more cores just
  starts the first 8, logged, not a hard failure.
- The sketch for the follow-up milestone: per-core `sched.current`
  becomes one new field on `PerCpuSlot`, turning `with_current_process`'s
  single line (`sched.current?`) into a per-core lookup with *zero*
  changes needed to any syscall handler built on top of it — confirmed
  during this milestone's own research, not merely hoped for.
