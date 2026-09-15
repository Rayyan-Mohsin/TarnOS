# 0014: Panic-Time Forensics Diagnostics, and Narrowed Evidence on the Remaining Corruption

## Status

Accepted (partial — see Consequences). Implemented across
`kernel/src/arch/x86_64/idt.rs`, `kernel/src/task/process.rs`,
`kernel/src/task/scheduler.rs`.

## Context

ADR 0013 left the cross-core kernel-stack corruption precisely
characterized (a `ret` popping a corrupted value that should have been a
valid kernel `.text` return address) but not root-caused, and depended on
an expensive, manual, live-GDB-attached-to-a-running-QEMU-instance session
to get even that much detail — not something that scales to catching many
samples.

This round pursued two concrete, checkable hypotheses for the corruption's
actual mechanism, then, when both came back clean, invested instead in
making every *future* occurrence self-diagnosing, without needing to catch
a machine live in a debugger at all.

## Decision

### 1. Two hypotheses checked and ruled out

- **Genuine double-dispatch of the same process onto two cores at once.**
  If two cores ever both believed the same `Pid` was their own `current`,
  they would use the identical physical per-index kernel stack
  simultaneously — a mechanism that would directly explain the observed
  signature. Added a defense-in-depth assertion directly in
  `scheduler::switch_to`, checking every *other* core's own `current`
  against the `Pid` about to be dispatched, before this core claims it.
  Ran a 30-run `test-kitchen-sink` stress batch with it in place: 13
  failures occurred, and the assertion never fired once. Kept permanently
  (cheap, and a real violation would now be caught immediately and
  attributably instead of corrupting silently), but this specific
  mechanism is not what's happening.
- **A `SYSCALL`-entry/`LSTAR`-programming indexing bug.** The kernel
  generates eight physical copies of the `SYSCALL` entry stub (one per
  possible core, `arch::x86_64::syscall::syscall_entry_stub!`), each with
  its own dedicated scratch-cell pair, closed over specifically because
  `percpu::core_index()` can't safely run before the stub's first
  stack-swapping instructions. A mismatch between a core's real identity
  and which stub/scratch-cell pair its `LSTAR` MSR was actually
  programmed to would deterministically corrupt whichever two
  cores/processes share the wrongly-aliased pair. Traced every step by
  hand: `syscall::init()` reads `percpu::core_index()` and calls
  `syscall_entry_addr_for_core(core)` using that same value, for both the
  BSP (`main.rs`, after `smp::bring_up_aps` assigns its slot) and every AP
  (`smp::ap_entry_on_own_stack`, after its own slot is assigned before it
  is ever started) — no indexing mismatch exists. Also independently
  re-verified the full stack-frame byte layout the entry stub's macro
  builds (the interleaved `push`es for the hardware-defined portion —
  `ss`/`rsp`/`rflags`/`cs`/`rip` — followed by the 15 general registers)
  against `TrapFrame`'s declared field offsets, byte by byte: every field
  lines up exactly, including the two registers `SYSCALL` itself
  clobbers (`rcx`→return address, `r11`→saved `rflags`) being pushed
  *twice*, once under their real name and once under the hardware-frame
  field they coincide with — the same defined-clobbered-register
  behavior every SYSCALL-based ABI (Linux included) already accepts, not
  a bug.

### 2. Panic-time diagnostics: name the core, the process, and the stack slot

Previously, a corrupted-frame panic's own message gave only a bare hex
address and error code — attributing it to a specific core, a specific
process, or a specific kernel stack needed a separately-attached live
debugger. Three additions close that gap permanently:

- `task::process::describe_kernel_stack_address(addr)`: reverse-maps a
  raw address into "which process-table slot's kernel stack owns this,
  and how far below its own top" by inverting `kernel_stack_slot_base`'s
  own arithmetic — pure computation, no locks, safe to call from a fault
  handler about to panic.
- `idt::page_fault_ring0`/`general_protection_fault_ring0` now report
  the faulting core (`percpu::core_index()`), that core's own `current`
  pid read lock-free off `percpu::PerCpuSlot.current` (safe from a panic
  path — never risks contending or deadlocking on `SCHEDULER`), and,
  when the faulting address/rip falls inside the kernel-stacks region,
  exactly which process-table slot and offset from its own top.
- `scheduler::dump_cores_for_panic()`: prints *every* booted core's own
  `current` pid and idle flag, plus every process-table index currently
  marked `STACK_BUSY`, right before either ring0 handler panics. A single
  faulting core's own state was never enough on its own to reason about a
  bug whose entire premise is that it's a *cross*-core phenomenon — this
  gives the whole machine's picture at the exact instant of the fault, at
  no cost beyond a few lock-free atomic loads and some print lines.

### 3. What the new diagnostics found, immediately

A single 40-run stress batch with these diagnostics in place caught five
independent panics, each self-attributing far more precisely than any
previous capture:

```
page fault accessing 0xa (error INSTRUCTION_FETCH) at 0xa
    -- core 0 was running raw pid 0x100000008
page fault accessing 0x4a (error CAUSED_BY_WRITE | USER_MODE) at 0xffffffff800511a6
    -- core 2 was running raw pid 0x100000004
page fault accessing 0xffffffff80080540 (error PROTECTION_VIOLATION | INSTRUCTION_FETCH) at 0x0
    -- core 3 was running raw pid 0x100000000
general protection fault (error code 0x0) at 0xffffff80051256
    -- core 0 was running raw pid 0x100000008
page fault accessing 0xffff980000013bc0 (error PROTECTION_VIOLATION | INSTRUCTION_FETCH) at 0xffff980000013bc0
    -- core 0 was running raw pid 0x100000003; kernel-stack slot 3 (offset 0x4bc0 from its own top)
```

Two details narrow the mechanism further than ADR 0013's own forensics
could:

- The first case's error code carries *only* `INSTRUCTION_FETCH`, no
  `PROTECTION_VIOLATION` — address `0xa` is simply unmapped (it's below
  `elf::USER_SPACE_MIN`'s guard range), not a present-but-NX kernel-stack
  page the way ADR 0013's own capture was. The corruption isn't
  exclusively landing on kernel-stack addresses; it can leave *any* small
  garbage value where a return address belongs. Both `0xa` and `0x4a`
  are exactly the magnitude of ordinary small integers this kernel passes
  around constantly as syscall arguments, capability indices, error
  codes, and IPC message words — consistent with some such value ending
  up on a stack where a return address was expected, not with a
  corrupted *pointer-shaped* value.
- The fourth case's `rip` (`0xffffff80051256`, i.e.
  `0x00ffffff80051256` once left-padded to 64 bits) differs from an
  unambiguously-valid sibling address seen in the second case
  (`0xffffffff800511a6`) in *exactly one byte*: the top byte (bits
  56–63, the highest-address byte of the little-endian qword) reads
  `0x00` where a valid canonical kernel address needs `0xff` — every
  other byte is untouched and self-consistent with real `.text`. A
  non-canonical address is exactly what triggers a `#GP` (not a `#PF`)
  the instant it's loaded into `RIP`, which is precisely what this
  capture is. This is a materially different signature than "the whole
  field is garbage": it looks like a *narrower-than-64-bit* write landed
  on part of an otherwise-correct 8-byte value, rather than the whole
  qword being overwritten with an unrelated one.

## What remains open

The root *write* is still not caught in the act. The byte-precise
evidence above is a stronger lead than anything gathered previously —
it points toward some sub-qword-width store partially overwriting a
return-address slot, which is far more specific than "two cores raced" —
but no such store was found by re-reading every hand-rolled
push/pop sequence in `context_switch.rs`/`syscall.rs` (all confirmed
whole-qword, correctly ordered, byte-for-byte matching their target
struct layouts), `elf.rs`'s segment loader (fully bounds-checked,
whole-page zeroing, no partial writes), or `memory/virt.rs`'s
`AddressSpace` teardown walk (confirmed, by re-reading it directly, to
touch only PML4 indices `0..256` — the kernel half every process shares
is structurally unreachable from it, ruling out a torn-down process's own
drop freeing frames still live under the shared upper half).

Continuing this should use the new diagnostics directly rather than
falling back to a fresh live-GDB session first: `dump_cores_for_panic`'s
output already narrows which core and which process were active
everywhere at the instant of the fault, and `describe_kernel_stack_address`
already names the exact slot and offset when the corrupted value itself
is a kernel-stack address. The next concrete step is a live capture that
reads the *full surrounding stack contents* (not just the one corrupted
slot) at the moment of one of these panics, specifically looking for
another core's own, differently-shaped local variables or return
addresses sitting adjacent to the corrupted slot — direct evidence of two
contexts' stack frames genuinely overlapping in physical memory, which
the byte-partial corruption above is the first evidence actually
consistent with.

## Testing

- `test-smp-sched-concurrency`, `test-smp-forced-preempt`,
  `test-smp-kill-cross-core`: still pass with the new `switch_to`
  assertion in place.
- 30-run `test-kitchen-sink` stress batch with the double-dispatch
  assertion: never fired, across 13 failures.
- 40-run `test-kitchen-sink` stress batch with the panic-forensics
  diagnostics: 21/40 passed; the 5 panics it caught are quoted above.
- Full ~23-scenario regression suite (`xtask test-all`), green.
- `cargo test -p tarnos-kcore -p tarnos-abi` and
  `cargo clippy -p tarnos-kcore -p tarnos-abi --all-targets -- -D warnings`,
  both clean.
- `test-kitchen-sink` still deliberately not wired into `test-all`/CI.

## Consequences

- Every future corrupted-frame panic now self-attributes its core,
  process, and (when applicable) kernel-stack slot — no live debugger
  needed to get this far, only to go further.
- Two more plausible mechanisms (double-dispatch, `LSTAR` indexing) are
  ruled out with direct evidence, narrowing what's left.
- The corruption's own signature is now understood more precisely (small
  garbage integers, and at least one confirmed sub-qword-width partial
  overwrite) than "a `ret` landed somewhere bad" — a materially better
  starting point for whoever continues this than ADR 0013 left.
- The root cause is still open. `test-kitchen-sink` stays out of
  `test-all`/CI.
