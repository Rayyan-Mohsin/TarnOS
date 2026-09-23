# 0028: Live Debugging Partially Unblocked; the LAPIC EOI Address Recurs as a Corrupted Code Pointer

## Status

Accepted. No source changes — this documents a debugging-methodology
finding and the evidence a corrected setup produced, per the user's
explicit choice to retry live debugging after ADR 0027 left four
guard-paging rounds and a static audit (`docs/adr/0027`'s own
Consequences) both coming up clean. Root cause remains open.

## Context

ADR 0022 reported live-debugging blocked: setting any hardware
watchpoint against this kernel's QEMU gdbstub target (`-smp 4`)
reliably desynchronized GDB from the real target on the next
`continue`, and its own Consequences said not to retry the same
approach without first resolving the tooling problem itself.

Re-reading ADR 0015 (a *working* live-debugging session, predating the
0022 regression) before retrying showed ADR 0022's own session never
mentions `-accel tcg,thread=single` — the exact flag ADR 0015
identifies as necessary to stop QEMU's multi-threaded TCG accelerator
from crashing (`qemu_mutex_lock_iothread_impl` assertion) when GDB
attaches to a multi-vCPU target, and which that same ADR reports made
"a dozen sessions" of hardware-watchpoint use reliable. This reads as a
methodology regression, not a new, permanent tooling wall — and ADR
0017's own scaling table independently makes it the right choice
anyway: `-smp 4` with `-accel tcg,thread=single` reproduces the
corruption at 75%, the highest rate of any configuration measured,
comfortably above plain `-smp 4`'s 50% and far above `-smp 2`'s ~1%
(ruling out the *other* standing suggestion, dropping to `-smp 2`, which
would have made a live session nearly useless for actually catching
anything).

## What the corrected setup actually did

A smoke test first: `-S`-halted boot, attach, `set language c`, watch
`percpu::SLOTS[0].lapic_id` (silent after boot's one legitimate
`assign_slot` write — the same target ADR 0022 chose), `continue`. It
fired exactly once, at the correct value (`0`, the BSP's own LAPIC ID),
and the target stayed fully controllable afterward — register reads,
further inspection, all worked. This confirms `-accel tcg,thread=single`
does fix the specific desync ADR 0022 hit.

It is not, however, fully reliable in this sandboxed environment:
across further attempts, one session reproduced ADR 0015's own
documented `qemu_mutex_lock_iothread_impl` assertion crash despite the
flag being present, and another set the identical watchpoint but never
reported a hit at all, letting the target run to a genuine, unrelated
panic with no live capture. Both are recorded honestly rather than
smoothed into "fixed" — live hardware watchpoints against this
GDB/QEMU pairing are *better* than ADR 0022 found, not dependable.

Given that, effort shifted to the technique ADR 0015 itself called the
safe, high-value default: post-mortem attachment, after a natural panic
has already halted the machine (this kernel's own `cli; hlt` loop, held
forever by `-no-shutdown -no-reboot`). A small driver script launched
fresh `kitchen-sink-test` instances back-to-back, released each from
its initial `-S` halt, waited for its serial log to show a
`[KERNEL PANIC]` line, and then opened a **second**, fresh
`target remote` connection for the actual inspection — deliberately not
reusing the release connection, since a `continue` past a `hlt` loop
never returns on its own (there is no debug trap to report), so trying
to `detach` afterward left a stale connection occupying the gdbstub's
single client slot and corrupted the next attach's protocol exchange
until this was fixed.

## Result: nine clean post-mortem captures, one recurring pattern

Nine of eleven total launches (81%, consistent with ADR 0017's 75%
figure for this exact configuration) produced a genuine panic, each
fully decoded: every core's registers and a symbolic backtrace via the
kernel's own loaded debug-info ELF. Most captures are unremarkable by
themselves — the same small-garbage-instruction-fetch and
kernel-stack-slot-adjacent `rip`/`rsp` signature this investigation has
tracked since ADR 0018, plus incidental register contents (an ASCII
`"KS_IPC__"`/`"OK"` fragment sitting in `rdx`/`r10`, a legitimate `Pid`
in `r12`) that are leftover output-formatting noise, the exact shape
ADR 0017 already explained away for `0x3333333333333333`.

One capture stands out. A page fault's faulting address was **exactly
`0xffff950000000000`** — `lapic::LAPIC_MMIO_VBASE`, offset zero, with
`PageFaultErrorCode(PROTECTION_VIOLATION | INSTRUCTION_FETCH)`: an
attempt to fetch an instruction from the LAPIC's own MMIO page (mapped
`NO_EXECUTE`, as device memory must be). A separate capture in the same
batch faulted at **exactly `0xb0`** — `lapic::REG_EOI`'s own raw offset
constant, with no base address at all. Both are clean, "someone meant
this" values, not random bit garbage — and both are fragments of the
exact same real expression this codebase computes in exactly one place,
`lapic::eoi()`'s `LAPIC_MMIO_VBASE + REG_EOI` (`0xffff9500000000b0`).
ADR 0015's own still-unexplained capture, from a completely separate
session months earlier, is a third data point in the same family:
`rdi` holding that identical combined address
(`0xffff9500000000b0`) at a ring0 fault.

Disassembling the actual generated code for `eoi`/`write_reg`/
`mmio_base` (none of these are inlined in this debug build — each is
its own symbol, its own `call`/`ret`) found nothing structurally wrong:
every `push`/`pop` and `sub $N,%rsp`/`add $N,%rsp` pair balances
correctly, the constant is built with an ordinary `movabs`, and nothing
here stores this address anywhere a return address or jump target could
later read it back from. This rules out a local calling-convention or
codegen bug in `lapic.rs` itself as the source — the value's *origin*
here is legitimate. Three independent captures landing on fragments of
this exact constant, in code that doesn't produce the fault itself, is
still evidence worth recording precisely rather than discarding: it
reads like a real (if still uncaught) wild write landing on a
particular return-address or function-pointer-shaped stack/memory slot
elsewhere, with this address's own bit pattern being an unusually
recognizable "victim" rather than the culprit.

## Testing

- No source changes; nothing to regress. Full 22-scenario suite was not
  re-run since the last commit already covers the unchanged tree.
- Tooling verification: one clean hardware-watchpoint hit against
  `percpu::SLOTS[0].lapic_id`, correct value, target remained
  controllable afterward.
- Eleven kitchen-sink-test launches (`-smp 4`, `-accel tcg,thread=single`),
  nine genuine panics, each fully decoded (all-core registers + symbolic
  backtraces via the loaded debug-info ELF) — two ran clean for the full
  60s watch window.
- `objdump` disassembly of `lapic::eoi`/`write_reg`/`mmio_base` in the
  built kernel, confirming balanced stack discipline and ruling out a
  local codegen bug as the source of the recurring address.

## Consequences

- ADR 0022's Consequences (don't retry live debugging without fixing
  the tooling first) is now superseded: `-accel tcg,thread=single` is
  the fix, confirmed working at least intermittently, and should be the
  default for any future live-attach session against this kernel —
  exactly what ADR 0015 already said, before ADR 0022's own session
  apparently dropped it. Live hardware watchpoints remain flaky enough
  in this sandboxed environment that post-mortem attachment (after a
  natural panic, per ADR 0015's own original recommendation) is the
  dependable technique to reach for by default; a live watch is worth
  trying opportunistically, not planning a session around.
- **Automation note for whoever runs this again**: releasing a target
  from an initial `-S` halt and then inspecting it post-mortem needs two
  *separate* gdbstub connections, not one continuous session with a
  `continue` then `detach` — `hlt` never reports a stop back to GDB, so
  a `continue` issued to release the halt blocks forever from GDB's own
  perspective; killing that first GDB *client* process (not the QEMU
  target) after it has sent the continue packet is what actually frees
  the target to run, and frees the single gdbstub client slot for the
  next, fresh post-mortem connection.
- The recurring `LAPIC_MMIO_VBASE`/`REG_EOI` address family — now three
  independent captures across two separate ADRs — is a specific,
  tractable signature to watch for in any future capture, even though
  this round's own disassembly rules out the address's own producing
  code as the bug. The natural next step, if this pattern recurs again,
  is a live watchpoint specifically on whichever stack slot or register
  a *fresh* capture shows holding this exact value shortly before a
  fault, now that hardware watchpoints are confirmed to work at least
  some of the time under the corrected accelerator flag.
- `test-kitchen-sink` stays out of `test-all`/CI. Root cause remains
  open.
