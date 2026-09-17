# 0022: Cross-Modifying-Code Hypothesis Refuted; Live Capture Blocked by GDB/QEMU Tooling

## Status

Accepted. No code fix landed this round — the plan's primary
hypothesis was tested and refuted, and its fallback live-debugging
step hit a genuine, reproducible tooling limitation rather than
producing a capture. Root cause remains open. See Consequences for
what should change about the approach in the next round.

## Context

ADR 0021 left the search narrowed to exactly two places, both already
audited byte-for-byte and found structurally correct: `syscall.rs`'s
`SYSCALL` entry stub, and the `switch_to`/`on_timer_tick` dispatch
logic it shares with the timer path. This round's own fresh reads
additionally confirmed `tarnos_kcore::RingBuffer` (proptest-verified
against a `VecDeque` reference model) and `interrupts::TICKS`
(a genuine `AtomicU64`) are not the cause.

Every capture across every ADR in this investigation has been a
`page_fault_ring0` or `general_protection_fault_ring0` — the CPU was
in ring 0 at the moment of the fault, never a ring-3 process fault.
This pattern had no explanation in any prior ADR. Re-reading
`Process::new_dummy` (`kernel/src/task/process.rs`) surfaced a
specific, previously unexamined mechanism that would explain it: it
copies a dummy process's compiled code out of the kernel's own live
`.text` into a freshly allocated frame, mapped into that process's own
address space — all performed by the BSP during boot — after which the
scheduler may dispatch that process onto any core, including one that
never performed the write. Per Intel SDM Vol. 3A §8.1.3, instruction
fetch is not guaranteed to observe a cross-core write without an
explicit serializing instruction on the fetching core — a hazard class
entirely distinct from the ordinary-data guarantees `SCHEDULER`'s lock
already provides. If a core fetched stale bytes that happened to
decode as a `syscall` instruction (plausible, since the dummy process's
own code *is* a `syscall` in a loop), the resulting ring-0 entry with
garbage register contents would explain the "always ring0" pattern no
other hypothesis in this investigation could.

One honest counterpoint was on record before testing: `IRETQ` is
itself a documented serializing instruction (SDM Vol. 3A Table 8-1),
and every ring-3 resume in this kernel already ends in `iretq` — on
real silicon this should already close the hazard. The plan treated
this as a reason to test cheaply rather than to skip the idea, since
QEMU's TCG backend's fidelity to this specific guarantee for cross-vCPU
code visibility was not something to assume either way.

## Step 1: the CMC hypothesis, tested and refuted

A single `cpuid` instruction (a genuine serializing instruction) was
inserted immediately before the final register-pop sequence at every
ring-3 resume point: `context_switch::resume()`, the ring-3 tail of
`timer_interrupt_entry`, the shared tail of `exception_entry_no_code!`/
`exception_entry_with_code!` (covering `lapic_timer_entry`,
`reschedule_entry`, and every process-facing fault entry), and
`syscall.rs`'s `syscall_entry_stub!` macro. Each insertion needed no
new clobber declarations: every register `cpuid` touches is
unconditionally overwritten by the pop sequence immediately following
it, and (for the raw `global_asm!` blocks) there is no Rust-level
operand tracking to violate in the first place.

Full 22-scenario regression stayed green. Measured against the
established pure-yield reproduction, 40 runs at the standard 17s
timeout: **32/40 (80%)** vs. the 36/40 (90%) baseline — well within
one standard error for this sample size, nowhere near the "drops to
~0%" the plan's own criterion required for confirmation.

**Refuted**, per the plan's own decision tree. This is consistent with
the architectural counterpoint noted going in: `IRETQ`'s existing
serialization guarantee most likely already covers this hazard, on
both real hardware and (evidently) under this QEMU/TCG configuration.
The experiment was reverted; nothing from Step 1 was committed.

## Step 3: live capture blocked by a genuine GDB/QEMU limitation

With Step 1 negative, the fallback was a corrected live-debugging
session — watching genuinely silent memory this time
(`percpu::SLOTS[0].lapic_id`, which should never change after boot's
`assign_slot` calls, and the shared dummy-process code virtual address
`0x400000`, which should never be written again after each process's
one-time setup) instead of the earlier round's mistake of watching a
field mutated on every dispatch.

This did not produce a capture. Setting *any* hardware watchpoint
against this QEMU gdbstub target (`-smp 4`, 4 vCPU threads) reliably
breaks the very next `continue`: GDB reports `Cannot execute this
command while the target is running` and then hangs for the full
timeout rather than exiting. Checking the target's actual state during
that hang (via a fresh `target remote` connection and `info threads`)
showed all 4 vCPUs had already run completely free, unsupervised, and
crashed into their normal panic-halt loops — meaning GDB's own
client-side run-state tracking desynchronized from the real target the
moment a hardware watchpoint was armed on this multi-vCPU connection,
silently losing any diagnostic value the watchpoint might otherwise
have provided. This reproduced with the barest possible script (a
single watchpoint, no conditions, no filtering) and independently of
which address was watched — it is not specific to the two addresses
chosen, and not the same "hot-path overhead suppresses the race"
problem ADR 0021 already identified. It is a distinct, more basic
tooling incompatibility: hardware watchpoints appear fundamentally
unusable against this specific GDB/QEMU version pairing once more than
one vCPU thread is present.

## Testing

- Full 22-scenario regression suite green after the CPUID experiment
  (before it was reverted as refuted).
- Pure-yield reproduction, `-smp 4`, 17s timeout, 40 runs: CPUID
  serialization 32/40 (80%) vs. baseline 36/40 (90%) — not
  significant.
- Live-capture attempt: no usable data; the tooling limitation itself
  is the reproducible result, confirmed with three independent script
  variants (with/without filtering conditions, with/without an
  explicit `interrupt`, one vs. two watchpoints) that all failed
  identically.
- No code change survived this round; the CPUID experiment and the
  pure-yield reproduction block were both reverted via
  `git checkout --` before this ADR was written.

## Consequences

- The cross-modifying-code hypothesis is closed. It was a genuinely
  strong fit for the "always ring0" pattern and every other piece of
  meta-evidence, and was worth the cheap test, but the measurement is
  unambiguous: this is not the mechanism.
- Live hardware-watchpoint debugging against this project's QEMU/GDB
  setup should not be attempted again without first resolving the
  tooling incompatibility itself (a newer QEMU or GDB build, or an
  alternative like a full-system record/replay tool, or scripting the
  connection through `gdbserver`'s own non-stop mode explicitly rather
  than relying on its default all-stop handling of a multi-vCPU
  target) — repeating the same approach will reproduce the same
  silent, uncapturable crash rather than new evidence.
- The search is still narrowed to `syscall.rs`'s entry stub and the
  `switch_to`/`on_timer_tick` dispatch logic (per ADR 0021), both
  already audited clean twice over. With CMC now also closed, the
  next productive avenue is most likely a fresh, adversarial line
  audit of `switch_to`'s exact decision sequence for a *logic* bug
  (not a lock-discipline or entry-stub-assembly bug, both already
  ruled out) — specifically what happens across the handful of reads
  and writes to `Inner` fields *within* a single `SCHEDULER.lock()`
  critical section, in an order that could produce a wrong answer for
  some interleaving of ready-queue state this investigation has not
  yet considered by name.
- `test-kitchen-sink` stays out of `test-all`/CI. Root cause remains
  open.
