# 0015: A Working Live-Debugging Setup, and Register-Level Corruption Evidence

## Status

Accepted (partial — see Consequences). No source changes; this documents
a debugging-methodology fix and the evidence it produced, for whoever
continues the still-open corruption from ADR 0013/0014.

## Context

Every prior live-debugging session against this kernel's QEMU instance
had two standing limitations, both noted in ADR 0012/0013: GDB's own `x`
(examine memory) command was completely unusable against this bare-metal
remote target, and no session had attached with the kernel's debug-info
ELF loaded as a symbol file, so every backtrace showed raw hex addresses
and `?? ()` instead of function names and source lines. This round found
and fixed the actual cause of both, and used the result to capture the
richest evidence yet — full symbolic backtraces and a decoded register
snapshot of a real corruption event — though the root *write* is still
not caught in the act.

## What was fixed

- **Attaching GDB to a multi-core QEMU instance while it's actively
  running (via `-s`, or the monitor's `gdbserver` command) reliably
  crashed QEMU itself**, with `ERROR:system/cpus.c:504:
  qemu_mutex_lock_iothread_impl: assertion failed:
  (!qemu_mutex_iothread_locked())`. This is a QEMU-internal bug in how
  its multi-threaded TCG (MTTCG) accelerator interacts with the gdbstub
  when more than one vCPU thread is live — not anything in this kernel.
  Passing `-accel tcg,thread=single` (forcing QEMU's single-threaded TCG
  mode, serializing all four emulated cores onto one host thread) made
  every attach reliable, including setting hardware watchpoints and
  `continue`ing past them repeatedly, with zero crashes across a dozen
  sessions this round. This is the same accelerator flag ADR 0010's own
  postmortem already used once to rule out a *different* hypothesis
  (genuine MTTCG host-thread races) — worth remembering as the default
  for *any* future live-attach session against this kernel, not just
  that one earlier investigation.
- **GDB's `x` command, and symbolic backtraces generally, were unusable.**
  Loading the kernel's own uninstalled, unstripped ELF (at
  `build/iso_root/boot/kernel` — built with debug info by default) via
  `file <path>` before attaching fixed both: `x/Ngx <addr>` now works
  directly, and `thread apply all bt` produces full Rust function names
  and source lines for every frame with a decodable frame pointer. One
  new wrinkle this introduced: once a Rust-language symbol file is
  loaded, GDB's expression parser switches to Rust syntax by default, so
  a C-style cast like `*(long*)0xADDR` fails with `No symbol "long" in
  current context` — `set language c` before setting a watchpoint or
  printing an expression restores the C-style casts every earlier
  session's notes already assumed.
- **A post-mortem attach (after the machine has already halted in its
  panic loop) is completely safe and needs neither of the above
  workarounds' urgency** — nothing is still running to race against, so
  this is the lowest-risk way to inspect a fresh panic's full register
  and stack state. Combined with this kernel's own `-no-shutdown
  -no-reboot`, a panicked machine sits in `cli; hlt` forever, giving as
  much time as needed to attach and inspect it.

## New evidence from a live-attached capture

Using the fixed setup (single-threaded TCG, symbol file loaded, attached
immediately after a real panic occurred organically — not a forced or
synthetic one), one fresh `page fault accessing 0x0 ... at 0x0` capture
(core 3, ring0) decoded as follows from its raw `FaultFrameWithCode`:

```
r12 = 0x0000000100000009   (Pid: index=9, generation=1)
rdi = 0xffff9500000000b0   (LAPIC_MMIO_VBASE + REG_EOI -- see arch::x86_64::lapic)
rdx = 0x3333333333333333
rax = 1, rsi = 4, rcx = 3, r13 = 2, r11 = 0x297
error_code = 0x10 (INSTRUCTION_FETCH only)
rip = 0x0, cs = 0x8 (ring 0)
```

Two things stand out:

- `rdi` holding exactly the LAPIC's EOI register address places this
  fault inside (or immediately after) a call to `lapic::eoi()` — the
  tail of `context_switch::ring3_reschedule`/`ring3_lapic_timer_tick`,
  which both call `on_reschedule_ipi`/`on_timer_tick` first (dispatching
  whatever process runs next) and only *then* call `eoi()`. `r12`
  holding a *different* `Pid` (index 9) than `dump_cores_for_panic`'s own
  concurrent read of `current[core]` (index 4, matching the panic
  message itself) is **not** a contradiction once this ordering is
  accounted for: `on_reschedule_ipi` computes `evicted_pid = Pid(9)`,
  clears then reassigns `current[core]` to whatever gets dispatched next
  (here, index 4), and returns — `r12`, a callee-saved register `eoi()`
  never touches, simply still holds the stale-but-legitimate
  `evicted_pid` value from before that reassignment. Concluded (after
  initially suspecting this was itself the smoking gun) that this part
  of the capture is fully explained by ordinary control flow, not a bug.
- `rdx = 0x3333333333333333` is the one field with no ordinary
  explanation. Nothing in this call path has any reason to hold a
  perfectly repeating single-byte pattern in a live register — it reads
  like either a deliberate poison/canary fill or literal ASCII (`'3'` is
  `0x33`) from some unrelated formatting/print routine, landing where a
  real value should be. Exactly the same *shape* of evidence as ADR
  0014's byte-partial corruption (a value that's part in real, own data
  and part something else's), just now caught live, in a register,
  rather than reconstructed after the fact from a saved frame.

## A negative result worth recording

Several watchpoints were set proactively on kernel-stack addresses
computed from *previous* panics' own corrupted locations (on the theory
that a fixed virtual address is deterministic across boots regardless of
which process occupies that slot). Every one of these fired — repeatedly,
across several separate sessions — but every single hit decoded to
entirely ordinary, correct, single-core activity: `scheduler::switch_to`
writing its own `kernel_stack_top` local, or its `assert_eq!` on
`pid.generation()`; `sys_spawn`'s inlined closure copying a freshly
constructed `Process` by value (debug builds don't guarangee copy
elision) between two stack slots *within the same slot's own range*.
None of this is a bug — it confirms that the specific byte-offset range
these panics keep landing in (roughly 1000–1500 bytes below a kernel
stack's own top) is simply a heavily-trafficked, entirely ordinary depth
for this kernel's own call graph (`entry stub -> dispatch -> scheduler
-> switch_to`), not a uniquely cursed address. Pre-registering a
watchpoint on a *guessed* address has a low hit rate for catching the
actual bug specifically because so much legitimate traffic already
passes through the same neighborhood — the technique works, but needs to
be aimed at an address only knowable once a specific run's own corruption
has already been observed (i.e., attach post-mortem, as above, not
pre-registered).

## What remains open

The root *write* — whatever puts a value like `0x3333333333333333` into
a live register or stack slot that should hold real kernel data — is
still not caught in the act. The working live-attach setup this ADR
documents removes the two biggest practical obstacles a future session
would otherwise re-discover from scratch. The most promising concrete
next step: attach post-mortem (as here) to several more fresh, organic
panics in a row, decode each `FaultFrameWithCode` fully the same way, and
look specifically for the same repeating-byte-pattern signature — if it
recurs with the *same* byte value, that pattern itself becomes a
tractable search target (`grep` the kernel and its dependencies for
anywhere that constant could originate); if it varies, that instead
argues for genuinely uninitialized memory being read before it's
written, which would point back at something like `Process`/`TrapFrame`
construction leaving a gap.

## Testing

- 50-run `test-kitchen-sink` stress batch with ADR 0014's panic-forensics
  diagnostics already in place: 20/50 passed. Panics captured this batch
  included the same small-garbage-integer and byte-partial signatures
  ADR 0014 documented, plus one whose `error_code` decoded to an
  impossible combination of reserved/exotic `PageFaultErrorCode` bits
  (`SHADOW_STACK | HLAT | SGX | RMP`), consistent with multiple fields of
  the same frame being corrupted at once, not a single isolated value.
- No source changes this round; nothing new to regress. `test-all`,
  `cargo test`/`clippy` were not re-run since the prior commit already
  covers the current tree.

## Consequences

- Future live-debugging sessions against this kernel should default to
  `-accel tcg,thread=single` and load the kernel's own debug-info ELF
  (`build/iso_root/boot/kernel`) as the symbol file before attaching —
  both are now proven fixes for standing obstacles two previous ADRs
  worked around or didn't fully solve.
- The corruption is now understood at the register level, not just the
  saved-frame level, for at least one capture — genuinely new resolution
  compared to ADR 0013/0014's own evidence.
- The root cause remains open. `test-kitchen-sink` stays out of
  `test-all`/CI.
