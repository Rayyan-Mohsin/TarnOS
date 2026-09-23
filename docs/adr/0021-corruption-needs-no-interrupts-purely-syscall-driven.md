# 0021: The Corruption Needs No Interrupts at All — Purely SYS_YIELD-Driven; Live Debugging Self-Defeating

## Status

Accepted. No code fix landed this round — every experiment is a
diagnosis, not a change, and all were reverted. Root cause remains
open. This ADR's value is in what it rules out and the one large,
positive narrowing it establishes — see Consequences.

## Context

ADR 0020 left two open leads from the approved plan's Step 3 fallback:
audit `percpu::core_index()` for fragility, and re-audit the raw
entry-stub assembly with the "needs process lifecycle" assumption
already disproven. Both audits found the existing code correct. This
round tested `core_index()`'s fragility directly (rather than only by
inspection) and then went further, testing whether the bug needs
interrupt-driven preemption *at all*.

## `core_index()` replaced with `GS_BASE`: no measurable effect

`core_index()`'s `CPUID` read + linear scan of `SLOTS` runs on every
hot dispatch path. Two replacements were tried:

- **`IA32_TSC_AUX`/`rdtscp`**: broke boot immediately (invalid opcode)
  — QEMU's default `qemu64` CPU model doesn't enable the `RDTSCP`
  CPUID feature bit. Reverted before any measurement.
- **`GS_BASE`**: a plain per-core MSR (`wrmsr`, no CPUID feature
  dependency), pointed at a distinct, pre-computed `u64` per core, read
  back via a single `mov reg, gs:[0]` — no serializing instruction, no
  scan. This booted correctly (22/22 regression green) and was
  measured against the pure-yield reproduction: **34/40 (85%)** vs. the
  established 36/40 (90%) baseline, both at a matched 17s timeout — not
  a significant difference (well under one standard error for n=40 per
  arm).

`core_index()`'s mechanism — CPUID+scan or GS-relative, doesn't matter
— is not the bug. This closes out ADR 0020's Step 3.1 fallback
target conclusively rather than just by inspection.

## The decisive result: zero interrupt-driven preemption still reproduces it

With `core_index()` ruled out, the only remaining Step-3-style target
was the raw entry-stub assembly itself. Rather than re-read it a third
time, this round ran a sharper experiment: disable *all* timer-driven
scheduling dispatch (`ring3_timer_tick` and `ring3_lapic_timer_tick`
both reduced to bookkeeping/EOI-only, matching `ring0_timer_tick`'s
shape) so the *only* way any process is ever redispatched is its own
voluntary `SYS_YIELD` syscall — no asynchronous interrupt ever
preempts a process mid-execution.

**Result: 19/20 (95%)** — at least as high as the interrupt-driven
baseline, not lower. The corruption needs no hardware-interrupt-driven
preemption whatsoever. It reproduces through pure, synchronous,
voluntary `SYS_YIELD` ping-pong alone.

This is the round's headline finding. It conclusively eliminates
`context_switch.rs`'s entire timer/exception entry-stub machinery as a
*cause* (it remains relevant only for *capturing* a fault report,
since ring0 fault handlers still run through it) and narrows the
search to exactly two remaining places, both already exercised by
`SYS_YIELD` alone: `syscall.rs`'s own hand-rolled `SYSCALL` entry stub,
and the `switch_to`/`on_timer_tick` (== `on_syscall_yield`) dispatch
logic both mechanisms share. Every previous round's byte-for-byte
audit of `syscall.rs`'s stub (ADR 0020) and of `switch_to`'s lock
discipline (this and prior rounds) already came back clean — but this
result proves those two are the *only* remaining places left to
distrust, which no earlier ADR could say with this level of certainty.

## A live-debugging attempt, and why it came back empty

With static audits exhausted, this round tried live debugging: QEMU's
gdbstub (`-s -S`) plus scripted GDB, setting hardware watchpoints on
`Inner.current[core]` (the exact field ADR 0018/0019's rsp-mismatch
evidence centers on) with a `commands` block that filters legitimate
`set_current`-originated writes and only stops on anything else.

The watchpoints armed and filtered correctly, but 45 real seconds of
GDB-supervised execution never reproduced the corruption at all — the
guest just ran, `set_current` fired legitimately and constantly (a
tight `SYS_YIELD` loop across 4 cores dispatches very often), and nothing
anomalous was ever caught.

This is itself informative, not just a dead end: `current[core]`
changes on *every single dispatch*, so watching it means trapping into
GDB on every single dispatch too — even though each trap's `commands`
block immediately decides "legitimate, continue," the trap itself adds
substantial overhead to the exact `SCHEDULER`-locked hot path this
session already proved (via the dispatch-trace-buffer's own atomic
counter, this session's earlier finding) is exquisitely sensitive to
added overhead: *any* slowdown of that path measurably suppresses the
race. Instrumenting the one field most directly tied to the bug's own
evidence is close to a worst-case choice for this specific bug — it
very plausibly suppressed the exact event being hunted for, the same
way the trace buffer's contended `seq` counter suppressed it from 50%
to 0/105 runs earlier this session.

The lesson carries forward: any future live-debugging attempt on this
bug must watch something that is normally silent (never legitimately
written after boot — `percpu::SLOTS`, a GDT/IDT region, a specific
process's kernel-stack guard page) rather than something on the hot
path itself, or the technique defeats its own purpose. A first attempt
at exactly this (`percpu::SLOTS[0].lapic_id`, which should never
change post-boot) was scripted but not completed this round — a raw
pointer-cast watch expression under GDB's Rust-mode expression parser
needed syntax iteration this round's time budget didn't allow finishing
cleanly.

## Testing

- Full 22-scenario regression suite green after the `GS_BASE`
  experiment (before it was reverted as ruled out).
- Pure-yield reproduction, `-smp 4`, 17s timeout, 40 runs per arm:
  `GS_BASE` core_index 34/40 (85%) vs. baseline 36/40 (90%) — not
  significant.
- Pure-yield reproduction, zero timer-driven preemption, `-smp 4`, 17s
  timeout, 20 runs: 19/20 (95%).
- No code change survived this round; every experiment was reverted
  via `git checkout --` before this ADR was written.

## Consequences

- `core_index()`'s CPUID+scan mechanism is confirmed, not just
  suspected, to be uninvolved. No further work needed there.
- The search is now conclusively narrowed to `syscall.rs`'s `SYSCALL`
  entry stub and the `switch_to`/`on_timer_tick` dispatch logic it
  shares with the timer path — both already audited clean by prior
  ADRs, meaning the next round needs genuinely new evidence (a
  non-hot-path live watchpoint, or a fresh pair of eyes on
  `SCHEDULER`'s own lock-acquisition/release sequencing under this
  specific narrowed lens) rather than another structural re-read.
- Any future live-debugging attempt must watch normally-silent memory,
  not a field mutated on every dispatch — instrumenting the hot path
  itself is now understood to risk suppressing this exact bug, not
  just slowing down the search.
- `test-kitchen-sink` stays out of `test-all`/CI. Root cause remains
  open.
