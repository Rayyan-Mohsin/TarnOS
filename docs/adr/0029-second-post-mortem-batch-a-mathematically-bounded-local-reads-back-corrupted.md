# 0029: A Second Post-Mortem Batch — a Mathematically-Bounded Local Reads Back Corrupted, Inside the Panic Path Itself

## Status

Accepted. No source changes — a second round of post-mortem captures
using ADR 0028's corrected methodology, run to see whether the
`LAPIC_MMIO_VBASE`/`REG_EOI` pattern recurs and to gather more data
points generally. Root cause remains open.

## Context

ADR 0028 left one recommendation live: if the `LAPIC_MMIO_VBASE`/
`REG_EOI` corrupted-code-pointer pattern recurs in a fresh capture, use
the now-partially-working hardware watchpoints to watch whichever
location holds it. A larger batch (15 launches, same
`-smp 4 -accel tcg,thread=single` configuration) was run to look for a
recurrence and broaden the evidence generally.

## Result: 11/15 panics, no exact recurrence, two new signatures

The `LAPIC_MMIO_VBASE`/`REG_EOI` exact-value pattern did not recur this
batch — it remains a real, three-times-observed signature (ADR 0015,
twice in ADR 0028) but evidently not a dominant or reliably-targetable
one; the corruption lands on many different values, of which that
family is only an occasionally-recurring, unusually recognizable
instance. No live watchpoint was set this round as a result — there
was nothing fresh to aim one at.

Two things from this batch are worth recording:

**Small-address *data* faults, not just instruction fetches.** Two
captures (`page fault accessing 0x6`, `accessing 0x13`) have
`error_code = 0x0` — no `INSTRUCTION_FETCH` bit set, meaning these are
ordinary data reads through a near-null pointer, not the "jumped to a
tiny address" signature this investigation has mostly seen. Both
`rip`s point at ordinary, legitimate-looking kernel `.text` addresses.
This is the same general "small/garbage pointer" class of evidence, but
the *data-read* half of it, previously under-represented compared to
the many corrupted-`rip` captures.

**A mathematically-impossible index, inside the panic-diagnostic path
itself.** One capture was a Rust-level panic, not a hardware fault:
```
core::panicking::panic_bounds_check(index=18446744071562569544, len=24)
  at task::scheduler::dump_dispatch_trace_for_panic (scheduler.rs:516)
```
`len=24` is exactly `TRACE_LEN`, `DISPATCH_TRACE`'s own correct,
compiled-in array bound — the array itself is fine. The index being
checked, `slot`, is computed two lines earlier as `(start + i) %
TRACE_LEN` (`scheduler.rs:512-514`) — a value that is mathematically
guaranteed to land in `[0, TRACE_LEN)` by construction, regardless of
what `start`/`i` themselves hold. For the bounds check to see this
astronomically large value instead, `slot`'s own storage — a plain
local, spilled to this debug build's stack between being computed and
being used, exactly like every intermediate value in the disassembly
ADR 0028 examined — must have been overwritten by something else
between the modulo and the array index. Decoded as a 64-bit value, it
sits almost exactly at `0xffffffff8007a788`-ish territory: the shape of
a real kernel `.text` address, not random bit noise, the same
"recognizable value, wrong location" signature the `LAPIC_MMIO_VBASE`
family showed.

Because `dump_dispatch_trace_for_panic` only ever runs from inside
`dump_cores_for_panic`, itself only reachable from a ring0 fault
handler already responding to an original, separate fault on this same
core, on this same kernel stack — the most likely reading is that this
is a *downstream* symptom of whatever already corrupted this core's
stack badly enough to cause its original fault, not proof of a second,
independent corruption event landing fresh on `DISPATCH_TRACE`'s own
territory. `DISPATCH_TRACE` itself is an ordinary, never-guard-paged
static, but guard-paging it would not address this specific capture:
the corrupted value is a stack-resident local, not anything living in
`DISPATCH_TRACE`'s own backing memory.

## Testing

- No source changes; nothing to regress.
- 15 kitchen-sink-test launches (`-smp 4`, `-accel tcg,thread=single`),
  11 genuine panics (73%, consistent with ADR 0017/0028's rates for
  this configuration), each fully decoded (all-core registers +
  symbolic backtraces).
- Confirmed `DISPATCH_TRACE`/`TRACE_LEN`'s actual source (`scheduler.rs`
  460-476) to verify the array bound itself is correct and unrelated to
  the corruption.

## Consequences

- The `LAPIC_MMIO_VBASE`/`REG_EOI` family remains real but is now known
  to be one recognizable instance among many possible corrupted values,
  not a dominant, reliably-reproducible target — planning a live-watch
  session specifically around waiting for that exact pattern to recur
  is a low-odds bet; any future live watchpoint attempt should instead
  watch whatever a *fresh* capture's own evidence points at, decided at
  the time, rather than pre-committing to this one value.
- This round's most informative capture reinforces, rather than
  contradicts, ADR 0027/0028's converging picture: corruption reaching
  a mathematically-bounded, purely local, stack-spilled value — this
  time inside the kernel's own panic-diagnostic path, a region no prior
  ADR's evidence had implicated — continues to argue for a wild write
  (or corrupted pointer used for one) landing on stack memory
  more or less anywhere a core happens to be executing, rather than a
  defect specific to any one data structure. Further guard-paging is
  very unlikely to be productive here specifically, since the corrupted
  value is not `DISPATCH_TRACE`'s own memory but a local spilled
  elsewhere on the same already-disturbed stack.
- `test-kitchen-sink` stays out of `test-all`/CI. Root cause remains
  open.
