# 0030: Milestone 10 — Last Base Level Check

## Status

Accepted. Implemented across nearly every file in the tree, in the
sense that Phase 1 alone read all of them; concrete diffs landed in
`kernel/src/arch/x86_64/{lapic,interrupts,gdt,percpu}.rs`,
`kernel/src/{memory/{mod,virt},driver/mod,task/{process,scheduler},main}.rs`,
`libs/tarnos-rt/src/heap.rs`, `xtask/src/main.rs`,
`.github/workflows/ci.yml`, five existing ADRs, and three new documents
(`README.md`, `docs/KNOWN-ISSUES.md`,
`docs/MILESTONE-10-LAST-BASE-LEVEL-CHECK.md`).

## Context

Milestones 1 through 9 each added a capability — boot, memory,
scheduling, IPC, SMP — and, across `docs/adr/0012` through `0029`, left
one long-running thread open: a cross-core scheduling corruption bug,
still not root-caused after nine ADRs' worth of investigation, now
contained behind a documented risk boundary rather than chased further.
Before stacking a filesystem and real drivers on top of the
process/scheduler/IPC/memory core as it stood, this milestone was a
single, deliberate pass over everything already built — the point in
the project where fixing something is still cheap, because nothing
downstream depends on it yet. `docs/MILESTONE-10-LAST-BASE-LEVEL-CHECK.md`
records the full plan; this ADR is its closing summary, matching every
prior milestone's own pattern of ending with a written record.

Explicitly not goals, per that plan: continuing the corruption
investigation itself, starting the filesystem or any driver, or a
rewrite of anything found. Every fix below is the smallest correct
change, not a refactor.

## Decision

### Phase 1 — Module-by-module correctness re-read

Read every file in the codebase's Scope (the whole kernel, all three
libs crates, all four userland crates, `xtask`, CI) in full, looking
specifically for what a targeted, feature-focused review tends to
miss: a doc comment describing behavior a later change quietly
invalidated. Found and fixed 15 instances of the same pattern — a
comment written during an early milestone, describing something as
"a later milestone task" or "doesn't exist yet," that had since become
permanent, completed infrastructure — including: `main.rs`'s own boot
sequence claiming kernel tasks and processes "aren't yet time-sliced
against each other" when `on_timer_tick` has unconditionally drained
the executor every tick since Milestone 2; `memory::mod.rs` claiming
`RSDP_REQUEST` was captured for a future ACPI/LAPIC migration that
actually went through Limine's own `MP_REQUEST` instead, never
touching ACPI; and `userland/init`'s own doc comment claiming that
process "has no heap," when `tarnos-rt`'s allocator is wired up
unconditionally for every binary that links it — `init` simply never
imports `alloc::*`. None of these were logic bugs; all were confirmed
safe via full regression before and after each fix.

### Phase 2 — Lint and dead-code cleanup

`cargo run -p xtask -- build` went from 8 warnings to 0. Two were
already-planned deletions (`memory::mod.rs`'s unused paging re-exports,
`lapic::REG_ID`/`this_lapic_id` — the latter's own doc comment claimed
a cross-check that was never actually wired up anywhere); the rest
(`percpu::spin_count`, `driver::Driver::name`, `memory::virt::translate`,
`task::process::Process::new_dummy`) are real but only exercised under
specific Cargo features, each given a targeted `#[allow(dead_code)]`
and a doc comment saying so, rather than a blanket crate-level
suppression. Also found and fixed a ninth, previously invisible
warning class: every `#[cfg(feature = "...-test")]` boot block that
ends by calling the diverging `scheduler::start()` makes the code
textually following it unreachable in that specific build — true for
13 of the kernel's test features, invisible before because CI's plain
`build` enables none of them and `xtask`'s own scenario runner never
captures build warnings. Fixed with one explained
`#[allow(unreachable_code)]` on `_start()` rather than restructuring
~14 mutually-exclusive blocks into an if/else chain.

Separately, ran `cargo clippy` against the kernel crate for the first
time ever (previously only `tarnos-kcore`/`tarnos-abi` were linted).
Found and fixed 5 findings (a manual modulo check, a
`type_complexity` warning resolved with a named type alias, three
`needless_range_loop` findings — each loop variable is printed in a
diagnostic or compared directly, not just used for indexing, so the
suggested `.iter().enumerate()` rewrite buys nothing) plus one in
`tarnos-rt`. Extended CI's clippy step to cover the kernel, `tarnos-rt`,
and every userland crate so this gap can't silently reopen.

### Phase 3 — Unsafe-code audit

Re-verified every `unsafe fn`/`unsafe {}` block's safety comment
against the current memory layout, with particular attention to the
raw `asm!`/`global_asm!` blocks in `context_switch.rs`, `smp.rs`,
`lapic.rs`, `percpu.rs`, and `syscall.rs`. Found one genuine, stale
safety comment: `lapic::read_reg`/`write_reg` claimed the LAPIC must be
reached via the kernel's HHDM mapping — directly contradicted by
`LAPIC_MMIO_VBASE`'s own doc comment a few lines above, which explains
the LAPIC is deliberately *not* HHDM-mapped, precisely because that
wrong assumption caused a real page fault this project already fixed
(recorded in `docs/adr/0009`). Every other unsafe block already
carried an accurate comment, or (for pure-register `asm!` like
`cpuid`/`out dx, al`/`cli`/`sti`/`hlt`) needed none, since correctness
there is already visible from the declared clobbers — a consistent,
deliberate pattern across the codebase, not an inconsistency.

### Phase 4 — Test and CI coverage audit

Confirmed every one of the 22 non-`kitchen-sink` `xtask` scenarios is
wired into both `test-all` and CI with no orphans in either direction,
and that `test-kitchen-sink` still runs correctly as its own standalone
entry point. Built the syscall coverage checklist the milestone plan
calls for (success path and boundary/error path per syscall, checked
against actual scenario code): 5 of 10 syscalls have real gaps in
boundary-path coverage (`SYS_SEND`/`SYS_RECV`'s `BadCapability`/
`QueueFull`, `SYS_SPAWN`'s `NoSuchProgram`, `SYS_PROCESS_START`'s and
`SYS_WAIT`'s own `InvalidTarget` rejection). All five are documented
with why they're lower-risk than they look (mostly already covered by
`tarnos-kcore`'s own proptest suite at the same underlying logic
layer) and deliberately deferred rather than hand-edited into the
delicate raw-`asm!` test fixtures this pass, with a concrete
recommendation for whoever picks them up.

### Phase 5 — Documentation consistency pass

Read all 29 ADRs end to end against current code. Found 5 places where
an ADR named an open gap that a *later* ADR actually closed, with no
forward-pointing note — `docs/adr/0006`'s physical-memory leak (closed
by `0007`), `0007`'s own predicted multi-core teardown hazard (which
came true and was fixed in `0018`), `0008`'s `sys_sbrk` lock-nesting
(reverted in `0011` once SMP made it a liveness problem), `0010`'s two
tripwires (`SYS_SEND`/`SYS_RECV`'s cross-core race and the missing
per-core timer, both closed in `0011`), and `0022`'s GDB/QEMU tooling
limitation (resolved in `0028`). ADRs 0012-0029 — the ongoing
corruption investigation — needed no such fixes: each is already an
explicit, correctly-sequenced continuation of the one before it, and
"still open" remains accurate since the bug genuinely still is.

Also added `README.md` — no top-level orientation document existed
before this milestone. Decided this was the right point to add one,
now that `docs/KNOWN-ISSUES.md` (Phase 6) gives it something concrete
to link rather than a vague pointer into 29 ADRs.

### Phase 6 — Known-bug containment and disclosure

Added `docs/KNOWN-ISSUES.md`: a single, findable summary of the
cross-core corruption bug for a future contributor who has not read
`docs/adr/0012` through `0029`. States plainly what it is, that it
requires multiple cores and sustained scheduling pressure to reproduce
at any practical rate (every `-smp 1` run across the whole
investigation has passed), that it always fails safely into a
controlled panic and halt, why `test-kitchen-sink` is excluded from
`test-all`/CI, and where the live investigation currently stands.

### Phase 7 — Interface readiness for filesystem/drivers

Checked three concrete questions against actual code, not assumption.
**Ready:** `elf.rs`'s loader is already fully data-source-agnostic
(needs no changes for a filesystem-backed `SYS_SPAWN`); the existing
`SYS_GRANT`/`CapTable` mechanism is directly reusable for propagating a
future filesystem-server capability down the process tree. **Missing:**
`KernelObjectRef` has exactly one variant (`Endpoint`) and `Rights`
exactly two bits — a device/file capability needs both extended;
`driver::Driver`/`CharDevice` are confirmed genuinely UART-shaped
(byte-oriented, no block addressing, no async completion) — a
`BlockDevice`-shaped trait needs designing from scratch, not extending;
`SyscallError` has no I/O-error variants yet. **Uncertain, flagged as a
decision for the next milestone's own planning:** whether drivers stay
kernel-resident or move out-of-process (the latter needs an entirely
unbuilt MMIO/IRQ-via-IPC primitive); whether a filesystem server needs
to grant capabilities to already-running unrelated clients, which the
current parent-to-`Suspended`-child-only `SYS_GRANT` cannot express.

### Phase 8 — Housekeeping

Confirmed task #100 (the one item this milestone's plan flagged by
name) was already closed correctly, before this milestone even began,
with an honest "superseded, not literally performed" description
matching what a fresh re-read of the ADR sequence independently
concludes. Spot-checked the other Milestone 9 investigation tasks for
the same concern; all accurately describe finishing that round's work
without overclaiming the bug was fixed. No other stale items found.

### Phase 9 — Final sign-off

`build/` and `target/` removed entirely and rebuilt from scratch:
`cargo run -p xtask -- build` clean, zero warnings. `cargo run -p xtask
-- test-all` green, 22/22, from that clean build. Both boot paths
verified explicitly: every non-UEFI scenario exercises BIOS,
`test-uefi-boot` passed within the same run. `cargo test -p
tarnos-kcore -p tarnos-abi` green (45 tests). `cargo clippy` clean
across the entire workspace, kernel included, from the same fresh
build. This ADR is the closing record.

## Consequences

- The base this milestone certifies — the process/scheduler/IPC/memory
  core — is the exact base filesystem and driver work now starts from.
  Nothing found this milestone needs revisiting before that begins.
- Zero build warnings and clean clippy across the whole workspace,
  including the kernel, for the first time — and CI now enforces both
  going forward, not just for the two crates it always covered.
- Every `unsafe` block in the kernel carries an accurate safety
  comment as of this commit; the one found stale is fixed.
- The cross-core corruption bug (`docs/adr/0012`-`0029`) remains open,
  by design — this milestone's Non-goals excluded continuing that
  investigation. It is now disclosed in one findable document
  (`docs/KNOWN-ISSUES.md`) instead of requiring a new contributor to
  read 18 ADRs to understand the risk.
- Five real, if low-risk, syscall boundary-path test gaps and three
  genuinely open design questions for the filesystem/driver milestone
  (Phase 7's "uncertain" items) are now named and sized rather than
  waiting to be discovered mid-milestone.
- The project has a top-level `README.md` for the first time.
