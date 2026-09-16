# 0019: Preemption Alone Reproduces the Corruption — the Address-Space Fix Was Necessary, Not Sufficient

## Status

Accepted. Implemented: `kernel/src/earlycon.rs`/`kernel/src/task/scheduler.rs`
(a real deadlock fix in the panic-diagnostic path itself). No fix for
this ADR's own headline finding yet — see Consequences.

## Context

ADR 0018 found and partially fixed a genuine address-space use-after-free
(freeing a process's `AddressSpace` before this core's own CR3 switched
away from it), cutting `test-kitchen-sink`'s failure rate from ~50% to
~10-12%. That ADR's own "what remains open" pointed at
`terminate_process`'s cross-core `SYS_KILL` finalize branch as the most
likely remaining instance of the same hazard.

This round tested that directly, per a plan reviewed and approved before
implementation: disable *only* the kill workload in `test-kitchen-sink`
(kill target + orchestrator; IPC, heap, lifecycle, and pressure all still
running) on top of ADR 0018's fixes, and re-measure.

**Result: 11/80 (13.75%) — statistically indistinguishable from the
10/80 (12.5%) full-scenario rate with kill enabled.** Disabling the kill
workload changed nothing. The cross-core `SYS_KILL` path is not the
(main) source of the residual corruption, and `terminate_process`'s own
lock-ordering reasoning (re-verified by re-reading the code during this
round's planning pass, and not disproven) is most likely already correct
as-is.

## A much smaller reproduction: pure preemption, zero process lifecycle

Following the plan's own contingency step, fresh captures from the
no-kill configuration were read directly. Both a `general protection
fault`/`page fault` targeting `arch::x86_64::smp::idle_stack_top_addr`-
style plainly-legitimate-looking addresses and one `index out of bounds:
the len is 16 but the index is 9932800` (a wildly corrupted index into a
16-element array, caught mid-loop inside `dump_cores_for_panic` itself —
a downstream artifact of the same corruption already having damaged this
core's own register/stack state) appeared, none involving `SYS_KILL`.

To isolate further, `test-kitchen-sink`'s entire workload was temporarily
replaced with 8 processes running nothing but
`kitchen_sink_tests::ks_kill_target_process` — an infinite `SYS_YIELD`
loop with **no spawn, no IPC, no heap growth, no exit, no kill,
ever**. Every process that exists at boot exists, unchanged, for the
entire run; nothing is ever created or destroyed; no `AddressSpace` is
ever torn down. The only thing happening at all is the LAPIC timer
preempting and redispatching these 8 already-existing processes across 4
cores, forever.

**This reproduced the corruption at 7/20 (35%)** — a *higher* rate than
`test-kitchen-sink`'s own full workload, in a scenario with zero process
lifecycle activity whatsoever. This conclusively rules out address-space
teardown (ADR 0018's own fix, or any variant of it) as the sole root
cause: there is nothing here for that fix to protect against, and the
corruption still happens. The bug is a separate, independent one, living
in the bare `on_timer_tick` → `switch_to` → `context_switch::resume`
redispatch path itself.

Every capture from this minimal scenario shows the same signature ADR
0018 first identified: the faulting core's `rsp` almost always lands
inside *some* process's kernel-stack slot, near its own top (offset
0x4700-0xe00 short of the 0x5000 slot size — consistent with a handful of
nested interrupt-entry frames deep). In some captures this slot matches
the core's own `current` pid exactly (the process is genuinely on its own
stack, but a corrupted value near its top gets fetched as if it were
code); in others it doesn't (current names one pid, `rsp` sits inside a
different one's stack entirely) — two distinct symptoms, not yet
distinguished as one mechanism or two.

## A second, real, unrelated bug found and fixed along the way

While reproducing the above, a single verification run appeared to hang
completely — no panic, no further output, indistinguishable from the
machine silently dying. It hadn't: `dump_cores_for_panic()` (called from
`arch::x86_64::idt`'s ring0 fault handlers, *before* `panic!()` ever
fires) prints through the ordinary `earlyprintln!`/`_println` path, which
takes `earlycon::COM1_TX_LOCK` with no timeout. `lang_items::panic`
already avoids exactly this failure mode — it calls `broadcast_panic_halt`
first and prints through `panic_println`'s bounded-spin-then-break-lock
fallback — but that protection only ever covered `panic!`'s own prints,
never `dump_cores_for_panic`'s, which runs earlier and grew substantially
across ADR 0014, 0017, and this session's own dispatch-trace/`rsp`
additions without ever being routed through the safe path.

Fixed: a new `panic_earlyprintln!` macro (routes through `panic_println`)
and an explicit `broadcast_panic_halt` call at the top of
`dump_cores_for_panic` itself (safe to call twice — lock-free,
idempotent). This is a real, independent bug — a diagnostic-path deadlock,
not a cause of the corruption itself — but a serious one on its own
terms: without it, a genuine crash under heavy multi-core contention could
be silently indistinguishable from a hang, exactly the failure mode this
whole investigation has been trying to get *away* from with better
diagnostics.

## Testing

- Full 22-scenario regression suite green after the deadlock fix.
- `test-kitchen-sink`, kill workload disabled on top of ADR 0018's fixes,
  80 runs, `-smp 4`: 11/80 (13.75%).
- The 8-pure-`SYS_YIELD`-process minimal scenario, 20 runs, `-smp 4`:
  7/20 (35%) genuine panics; the other 13 runs correctly hit their
  15-second timeout with no panic, which is the *expected* outcome for
  this scenario (none of the 8 processes ever call `SYS_EXIT`, so a
  healthy run never halts on its own) — not evidence of an additional
  hang, once distinguished from the deadlock above.

## Consequences

- ADR 0018's fix was necessary (it closed a real, independently
  confirmed bug and cut the failure rate roughly 4-5x) but not
  sufficient. `terminate_process`'s "not found anywhere" branch is very
  likely *not* the remaining culprit — Step 2 of the previously-approved
  plan (a `STACK_BUSY`-gated wait there) is deprioritized pending
  evidence it's actually needed, rather than implemented speculatively.
- The real remaining root cause is now known to live in the bare
  preemption/redispatch mechanism itself
  (`on_timer_tick`/`switch_to`/`context_switch::resume`/the raw entry-stub
  assembly), reachable with zero process creation or destruction — a
  fundamentally different, narrower search than every previous ADR in
  this investigation (0012 through 0018) assumed. The 8-process pure-yield
  configuration is the smallest, highest-rate (35%) reproduction found to
  date and should be the starting point for the next round, using the
  now-fixed diagnostics (which no longer risk hanging instead of
  reporting).
- Two concrete next steps for that round: (1) determine whether the
  "rsp inside a *different* process's stack" and "rsp inside its own
  stack but with corrupted top-of-stack content" captures are the same
  mechanism or two; (2) audit `gdt::set_kernel_stack`/
  `syscall::set_syscall_kernel_stack`'s shared dependency,
  `percpu::core_index()` (a `CPUID` read plus a linear scan of `SLOTS` on
  *every* call, from every hot scheduling path) for any window in which
  it could return the wrong core's index under concurrent, high-frequency
  calling.
- `test-kitchen-sink` stays out of `test-all`/CI. Root cause remains open.
