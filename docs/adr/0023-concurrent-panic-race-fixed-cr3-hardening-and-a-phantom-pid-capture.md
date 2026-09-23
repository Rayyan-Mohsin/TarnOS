# 0023: Concurrent-Panic UART Race Found and Fixed; CR3 Hardening Added; a Clean "Phantom Pid" Capture

## Status

Accepted. One genuine, permanent bug fixed this round (not the
root-cause corruption itself, but a real, independent defect in the
kernel's own panic-reporting path that was actively destroying
evidence). Additional defense-in-depth hardening added to `switch_to`.
Root cause of the underlying cross-core corruption remains open, but
this round's fix unblocked dramatically cleaner diagnostic captures,
one of which is the most structurally specific evidence this
investigation has produced yet. See Consequences for the resulting
next hypothesis.

## Context

The previous round's `switch_to` hardening (four new invariant
assertions, added but not yet committed) had already produced the
first genuine "caught in the act" evidence this investigation had ever
recorded: a `scheduler.rs` read-back assertion (TSS.RSP0 immediately
after `gdt::set_kernel_stack`) fired on a real stress run. Its message
was truncated by the batch's per-run timeout before the interesting
values could print, and a follow-up batch with a longer timeout failed
to reproduce that specific assertion again, but surfaced a severely
garbled, interleaved final diagnostic line — multiple cores' own
`[panic-dump]` output spliced together mid-word, including an
obviously-corrupted, astronomically large value where a small integer
was expected. That garbling was flagged as worth investigating in its
own right, since a corrupted *diagnostic print* and a corrupted *piece
of real kernel state* are very different findings.

## Found: a genuine concurrent-panic race in the kernel's own reporting path

Investigating the garbled output directly (rather than re-running more
batches and hoping) found a real, previously unnoticed defect:
`task::scheduler::dump_cores_for_panic` is called directly from
`idt.rs`'s ring0 fault handlers — `page_fault_ring0`,
`general_protection_fault_ring0`, `double_fault_handler` — **before**
the eventual `panic!()` ever reaches `lang_items::panic`, which is the
only place that calls `lapic::broadcast_panic_halt` to stop every other
core. If two cores fault within a few instructions of each other —
exactly the shape a shared-state corruption bug tends to produce, and
exactly what this investigation has been chasing — both can reach a
diagnostic print before either one's halt IPI is actually serviced by
the other, and both then contend for `earlycon::COM1_TX_LOCK`.

This is made *worse*, not better, by `earlycon::panic_println`'s
existing bounded-spin-then-`break_lock` design (itself a fix from an
earlier round, for a different, single-core deadlock hazard): under
QEMU/TCG, a tight `core::hint::spin_loop()` loop can complete its
10,000,000-iteration bound *faster* than the handful of slow,
VM-exiting `out dx, al` port writes the other core's in-progress line
still needs to finish. The result is that the "safety" fallback forces
the lock open on a write that is still genuinely, legitimately
in-progress, producing exactly the byte-interleaved, truncated output
observed: `run_12.log` (from the previous round) showed two
overlapping `[panic-dump] core 0: ...` lines, the first cut off
mid-write immediately ahead of that round's own load-bearing evidence
(the `scheduler.rs:574` assertion) getting truncated the same way.

### The fix

`lang_items.rs` now has a single, machine-wide "who gets to report this
panic" election (`claim_panic_reporter`, backed by one
`AtomicUsize` CAS): the first core to call it wins, sends
`broadcast_panic_halt` itself, and proceeds; every other core halts
immediately, before it can print anything at all. `idt.rs`'s three
ring0 fault handlers now call this as their very first action, ahead
of `dump_cores_for_panic`; `lang_items::panic` calls it in place of its
previous direct `broadcast_panic_halt` call (a same-core re-entrant
call correctly still returns `true`, so a fault's `idt.rs` handler and
the `panic!()` it eventually raises both see themselves as the
reporter). This is lock-free and cannot itself get stuck, matching
`broadcast_panic_halt`'s own existing design constraints.

## Result: dramatically cleaner captures, and one genuinely new lead

Three 40-run pure-yield batches (`-smp 4`, 17s timeout) were run after
this fix, rebuilding and re-verifying the ISO each time. Panic rates
were 31/40, 22/40, and 36/40 — noisy, as this investigation's rate has
always been batch-to-batch; no claim is made that the fix changed the
underlying rate, only that every capture across all three batches was
now complete and legible. Two specific captures stand out:

**A genuine double fault with a fully clean backtrace.** RIP
`0xffffffff80021bb9` resolved (via `objdump`) to the very first `pop`
instruction in `syscall_entry_3` — immediately after `mov rax, rsp`,
itself immediately after `call syscall_dispatch`. This is the exact
point where the assembly trusts whatever `*mut TrapFrame` Rust handed
back as the new stack pointer and begins popping the resumed process's
saved registers from it. `switch_to` returns
`&mut process.trap_frame as *mut TrapFrame` — a field embedded directly
in the `Process` struct, not derived from `kernel_stack_top` at all, and
therefore never covered by last round's TSS.RSP0/SYSCALL-scratch-cell
read-back checks. This is the most mechanistically specific fault
captured in the whole investigation: a resume that double-faults on its
very first stack access, immediately after `process.address_space.activate()`
(a CR3 write) ran moments earlier in the same function.

Two candidate explanations for *why* were checked and one closed:
`AddressSpace::new` copies the kernel half (PML4 256..511) from the
boot-time table *once*, at creation time, which would only matter if
some kernel-owned memory `process.trap_frame` depends on were mapped
*after* a given process's address space already existed. Both
candidates that could cause that — the kernel heap and per-process
kernel stacks — are already eagerly, fully mapped before the first
`AddressSpace::new()` ever runs (`heap::init`'s single up-front mapping
loop; `task::process::init_kernel_stacks`, whose own doc comment
already names avoiding exactly this class of bug as the reason it's
eager rather than lazy). That avenue is closed for this specific
field. What remains open is whether `process.address_space.pml4_frame()`
itself was ever wrong at the moment of `activate()` — untested until
now, since (as with TSS.RSP0 last round) nothing verified `activate()`'s
own CR3 write actually stuck.

**Fixed (hardening): a CR3 read-back assertion**, added immediately
after `process.address_space.activate()` in `switch_to`, mirroring the
exact pattern already used for TSS.RSP0 and the SYSCALL scratch cell:
`assert_eq!(Cr3::read().0, process.address_space.pml4_frame(), ...)`.
Full regression stayed green (22/22); a follow-up 40-run batch with
this check active did not trigger it, so CR3 itself reading back wrong
is not (yet) confirmed as a live mechanism — but the gap is now closed
the same way the RSP0 gap was, and any future occurrence will name
itself immediately rather than manifesting as an unattributed double
fault three instructions later.

**A clean "phantom pid" capture — the single most structurally
specific finding of this investigation.** In the same CR3-hardened
batch, one run hit `switch_to`'s pre-existing (not new)
`panic!("switch_to named a process that does not exist")` — the match
arm reached when `sched.generations[index] == pid.generation()` passes
but `sched.processes[index]` is not `Slot::Occupied`. This had never
been seen fire cleanly before. The capture shows no `[panic-dump]`
lines at all (this panic path doesn't call `dump_cores_for_panic`) and,
critically, occurred *before* the boot log's own
`"[boot] kitchen-sink-test: spawned all orchestrator + pressure processes"`
line had printed — meaning it happened while the BSP was still inside
the boot-time loop spawning the 8 pure-yield processes one at a time,
with another core already far enough along to be dispatching one of
the earlier ones.

`spawn()` itself was re-audited and is correctly atomic: it sets
`sched.processes[index] = Slot::Occupied(..)` and pushes to
`sched.ready` under one `SCHEDULER.lock()` acquisition, so another core
can never observe the ready-queue entry without also observing the
Occupied slot through the same lock. `allocate_pid()` was also
checked: every index's `generations[index]` is incremented *before*
that generation is ever handed out (`wrapping_add(1)` precedes
`Pid::new`), so a freshly spawned process's generation is always ≥1,
not 0 — ruling out the simplest version of "an unused slot's default
generation coincidentally matches a fresh pid's." The actual mechanism
is therefore not a plain ordering bug in `spawn()`, and reaching this
panic requires a `Pid` value — somewhere between the ready queue and
`switch_to`'s own read of it — whose `index` names a slot this
reproduction never spawned into, while its `generation` field
independently reads back as a match for that (unrelated) slot's
current counter. This is a different, more specific shape than every
prior "garbled scalar" observation: it is not a wild address or an
obviously-impossible bit pattern, but a **plausible-looking, internally
self-consistent, wrong `Pid`** reaching the scheduler's own dispatch
path. The existing check caught it and halted safely rather than
resuming garbage — exactly what it exists for — but its trigger is now
a concrete, reproducible target for the next round.

## Testing

- `cargo run -p xtask -- build`: clean (only pre-existing dead-code
  warnings).
- Full 22-scenario regression suite: green, run twice independently
  (once after the panic-reporter fix, once again after the CR3
  assertion was added) — no new failures, confirming neither change
  perturbs any correct-path scenario.
- Pure-yield reproduction, `-smp 4`, 17s timeout: three 40-run batches
  post-fix (31/40, 22/40, 36/40) — every capture across all three was
  complete and non-interleaved, a first for this investigation.
- Every ISO build verified via its own boot-log line and
  `strings ... | grep -c "pure-yield process"` before trusting a batch
  against it, per this investigation's standing methodology.

## Consequences

- The concurrent-panic UART race is a real, independent, now-fixed
  defect in its own right, unrelated to (but actively obstructing
  investigation of) the underlying corruption. `claim_panic_reporter`
  is permanent, not a temporary experiment.
- The CR3 read-back assertion is permanent hardening, added on the
  same reasoning and in the same style as last round's TSS.RSP0/SYSCALL
  scratch-cell checks. It has not yet fired; if it ever does, treat
  that as direct, load-bearing evidence the way this round's
  TSS.RSP0/"does not exist" hits already have been.
- The double-fault-at-first-resume-pop capture rules out stale/lazy
  kernel-half page-table mappings as the cause for `process.trap_frame`
  specifically (both the heap and kernel stacks are confirmed eager),
  but leaves open whether `activate()`'s CR3 write itself, or
  `process.address_space.pml4_frame()`'s own stored value, is ever
  wrong — now covered by the new assertion, so the next occurrence
  will self-report.
- The "phantom pid" capture is the most promising concrete lead handed
  to the next round: the corruption's effect on `Pid` values reaching
  `switch_to` is not always an obviously-wild bit pattern — it can be a
  plausible, self-consistent value naming the wrong slot. The next
  productive step is auditing every write path that can put a `Pid`
  into `sched.ready` or hand one to `switch_to` (not just `spawn`,
  already cleared, but `wake_blocked_process_locked`,
  `on_reschedule_ipi`, and any other producer) for a case where the
  `Pid` value itself — not the table it's checked against — is read
  from stale, reused, or not-yet-fully-written memory.
- `test-kitchen-sink` stays out of `test-all`/CI. Root cause of the
  underlying corruption remains open.
