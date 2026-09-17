# 0020: Double-Preemption-Source Hypothesis Refuted; a Real Diagnostic Bug Fixed; Root Cause Still Open

## Status

Accepted. Implemented and committed:
`kernel/src/arch/x86_64/idt.rs` (a genuine, permanent diagnostic fix).
No fix for the underlying corruption itself — see Consequences.

## Context

ADR 0019 narrowed the still-open cross-core corruption bug to the bare
`on_timer_tick`/`switch_to`/`context_switch::resume` redispatch path,
reproducible with zero process-lifecycle activity (8 processes doing
nothing but `SYS_YIELD` forever), at 7/20 (35%) under `-smp 4`.

Re-reading `context_switch.rs` alongside `interrupts.rs`/`lapic.rs`
surfaced an untested architectural asymmetry: the BSP is the only core
with two independent preemption sources feeding
`scheduler::on_timer_tick` — the legacy PIT (`ring3_timer_tick`, which
can only ever route to the BSP) *and* its own per-core LAPIC timer
(`ring3_lapic_timer_tick`, armed identically on every core since
Milestone 8). A plan was written and approved to test this directly:
temporarily strip `ring3_timer_tick` down to bookkeeping+EOI only
(matching `ring0_timer_tick`'s shape), leaving the LAPIC timer as the
BSP's sole scheduling-relevant preemption source, and re-measure the
pure-yield reproduction.

## A methodology error, caught before it produced a wrong conclusion

The first attempt at this measurement used a 17-second per-run timeout
(2 seconds more than ADR 0019's original 15s, added to give a
just-started panic dump more time to flush its output before a hard
`SIGTERM` truncates it) and got 14/20 (70%) — compared naively against
ADR 0019's 7/20 (35%) baseline, this looked like the experiment made
things *worse*, doubling the failure rate.

That comparison is invalid: a longer per-run window gives the
underlying race strictly more wall-clock time to manifest, so a higher
rate at 17s proves nothing against a rate measured at 15s. Re-measuring
the *unmodified* baseline at the same 17s timeout gave 36/40 (90%) —
already far higher than the old 15s-timeout 35% figure, confirming the
timeout window itself, not the code change, explains most of the
apparent shift. Every comparison in this ADR uses a single, fixed 17s
timeout on both sides.

## The hypothesis, properly tested

With both sides measured at 17s, `-smp 4`, 40 runs each:

- Baseline (unmodified `ring3_timer_tick`): 36/40 (90%)
- Experiment (`ring3_timer_tick` stripped to bookkeeping+EOI only): 33/40 (82.5%)

The 7.5-point gap is within one standard error of the difference
(≈7.7 points for n=40 per arm) — not statistically distinguishable from
noise. **The double-preemption-source hypothesis is refuted.** The
BSP's redundant PIT-driven scheduling call is not a meaningful amplifier
of the corruption; removing it neither fixes nor measurably worsens it.
The temporary experiment was reverted (`git checkout --`), never
committed.

## A real, permanent diagnostic bug found and fixed along the way

While auditing `context_switch.rs`'s and `syscall.rs`'s hand-rolled
entry stubs as this round's fallback investigation (Step 3 of the
approved plan), re-deriving the exact byte layout `page_fault_ring0`/
`general_protection_fault_ring0` assume exposed a real bug in the `rsp`
diagnostic ADR 0018 added: those two handlers only ever run when the
interrupted context was **already in ring 0** — a same-privilege
exception. Per the SDM (Vol. 3, 6.13), a same-privilege exception never
pushes RSP/SS onto the stack at all; those two words only exist when
the exception also raises the privilege level. Reading `(*frame).rsp`
in the ring0 handlers therefore dereferenced whatever stale bytes
already happened to sit on the stack below the frame the CPU actually
pushed — not a real value.

This means **every "current names one pid, rsp sits inside a different
one's stack" observation in ADR 0018 and ADR 0019 was comparing a
genuine `rip` against noise**, not real evidence about which stack the
faulting core was actually on. The "two distinct symptom patterns" ADR
0019 flagged as unresolved was, at least in part, an artifact of this
bug rather than two genuine mechanisms.

Fixed by computing the real fault-time `rsp` as a pure address
computation instead of a memory read: `frame`'s own address plus the
byte offset the (unwritten) `rsp` field occupies is exactly where the
CPU's real `rsp` was pointing right before it took the exception, since
a same-privilege exception never moves the stack. This is a genuine,
permanent correctness fix (to a diagnostic, not to runtime behavior —
the buggy field was never used for anything but panic-message text) and
is committed independently of this round's experiment.

## Structural audits: no further bugs found

Two full audits were done as this round's Step 3 fallback, both
negative results:

- **`percpu::core_index()`/`SLOTS`/`assign_slot`**: re-derived the
  ordering argument (`assign_slot` runs once per core, strictly before
  that core ever starts; `SLOTS` is read-only after boot) and found it
  sound. No window where two slots could transiently report the same
  LAPIC ID, and no stale-default collision is possible.
- **`context_switch.rs`'s and `syscall.rs`'s raw entry-stub assembly**:
  recomputed every push/pop offset by hand for both the timer stub and
  the (previously un-audited this session) `SYSCALL` entry stub against
  the `TrapFrame`/`FaultFrameWithCode` struct layouts, byte by byte.
  Both match exactly. The `SYSCALL` stub's double `push rcx`/`push r11`
  (once for the hardware-clobbered rip/rflags, again for the
  general-purpose register slots) initially looked suspicious but is
  correct, standard x86_64 `SYSCALL` ABI behavior: the instruction
  itself destroys the caller's original `rcx`/`r11`, which is exactly
  why the calling convention `docs/adr/0004` already documents uses
  `r10` instead of `rcx` for the fourth argument, and exactly why
  `kitchen_sink_tests::ks_kill_target_process`'s own inline `syscall`
  already declares `out("rcx") _, out("r11") _`. Also confirmed
  `TSS.RSP0` (`gdt::set_kernel_stack`) and the per-core `SYSCALL`
  kernel-stack scratch cell (`syscall::set_syscall_kernel_stack`) are
  both updated, correctly and core-locally, inside `switch_to` before
  any resume — no stale-stack-pointer window found there either.

## Testing

- Full 22-scenario regression suite green after the `idt.rs` fix.
- Pure-yield reproduction, `-smp 4`, 17s timeout: baseline 36/40 (90%),
  `ring3_timer_tick`-stripped experiment 33/40 (82.5%) — not a
  significant difference.
- `test-kitchen-sink` itself untouched this round (no code change
  survived to test against it).

## Consequences

- The BSP's dual PIT+LAPIC preemption sources are not the amplifying
  mechanism. This architectural asymmetry could still be simplified
  later purely for clarity (the PIT's scheduling call is now known to
  be redundant, not merely suspected), but doing so is not a fix for
  anything and was correctly left uncommitted.
- The `rsp`-attribution fix means the **next** captured panic is the
  first one whose `rsp` evidence can actually be trusted. Any future
  round should re-capture fresh panics under the pure-yield reproduction
  and re-evaluate the "same stack, corrupted value" vs. "wrong stack
  entirely" question from ADR 0019 with real data, not re-use the old
  captures.
- Two major suspect code paths — the timer/exception entry stubs and
  the `SYSCALL` entry stub, including their respective per-core
  kernel-stack-pointer update mechanisms — have now been audited
  byte-for-byte and are structurally sound. The search should move away
  from "is the entry/exit assembly correct" (repeatedly confirmed, now
  across both entry mechanisms) toward re-examining `SCHEDULER`'s own
  locking discipline and the process table's `Slot`/generation
  bookkeeping under genuinely concurrent, high-frequency dispatch — the
  one area not yet given a dedicated adversarial pass this session.
- The standing methodology lesson: **a stress-test comparison is only
  valid at a fixed per-run timeout.** This is now an explicit, permanent
  addition to this investigation's discipline, alongside the
  already-established rules (verify each ISO's boot-log line, never run
  two QEMU processes against the same ISO concurrently, always revert
  temporary experiments before committing).
- `test-kitchen-sink` stays out of `test-all`/CI. Root cause remains
  open.
