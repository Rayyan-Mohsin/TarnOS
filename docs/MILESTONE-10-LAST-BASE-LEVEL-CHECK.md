# Milestone 10: Last Base Level Check

## Why this milestone exists

Every milestone so far (1 through 9) added a capability: boot, memory,
scheduling, IPC, SMP, and — across ADRs 0012 through 0029 — a long,
still-unresolved investigation into a cross-core scheduling corruption
bug, now contained behind guard pages and excluded from normal builds
and CI. The next milestones (a filesystem, real drivers, more of a
userland) will all be built *on top of* the kernel as it exists right
now — the process/scheduler/IPC/memory core, not just the newest
feature.

This milestone adds no new capability. It is a single, deliberate pass
over everything already built, before more is stacked on top of it —
the point in the project where going back and fixing something is
still cheap, because nothing downstream depends on it yet.

## Scope

Everything currently in the tree:

- `kernel/src/arch/x86_64/` — `gdt`, `idt`, `interrupts`, `lapic`,
  `percpu`, `smp`, `syscall`, `context_switch`
- `kernel/src/memory/` — `phys`, `virt`, `heap`
- `kernel/src/task/` — `process`, `scheduler`, `executor`
- `kernel/src/ipc/` — `capability`, `endpoint`, `message`
- `kernel/src/driver/` — `mod`, `uart`
- `kernel/src/elf.rs`, `sync.rs`, `earlycon.rs`, `lang_items.rs`,
  `main.rs`
- `libs/tarnos-abi`, `libs/tarnos-kcore`, `libs/tarnos-rt`
- `userland/init`, `echo-child`, `exit-code-child`, `heap-child`
- `xtask`, `.github/workflows/ci.yml`
- All 29 existing ADRs, for accuracy against the current code

## Non-goals

- **Not another round of the corruption investigation.** ADRs 0012-0029
  are a separate, ongoing thread with their own task (#108's successor,
  whenever the LAPIC lead or another one is picked back up). This
  milestone treats that bug as a known, documented, contained risk —
  see Phase 6 — not something to re-open here. If this pass happens to
  surface a genuinely new, concrete lead, record it and hand it to that
  thread; don't chase it inline.
- **Not starting the filesystem or any new driver.** This milestone
  ends when the base is verified solid, not when new capability exists.
- **Not a rewrite.** Every finding gets fixed with the smallest correct
  change, the same discipline every ADR in this project has already
  used — no speculative refactors, no new abstractions the current code
  doesn't need.

## Phases

Each phase has a concrete stopping condition — something checkable, not
"look busy for a while." Work them roughly in order; later phases
assume earlier ones are clean.

### Phase 1 — Module-by-module correctness re-read

Read every file listed under Scope, in full, with fresh eyes, looking
specifically for what a *targeted* review (one focused on the feature
being added at the time) would tend to miss: a stale doc comment
describing behavior a later refactor changed, a helper that's now only
called from one place and could be simplified, an invariant relied on
in one file that's stated (or not stated) in another. This project's
own style already documents *why* behind almost everything — the
review is checking that those "why"s are still true, not re-deriving
them from scratch.

Concrete items already known from this pass's own reconnaissance,
worth confirming or fixing directly rather than rediscovering:

- `kernel/src/task/process.rs`'s `trap_frame_for` and `scheduler.rs`'s
  `ProcessBox` doc comments both narrate the guard-paging history
  (ADR 0025-0027) — check they still match the code exactly now that
  the dust has settled, not just at the moment each ADR landed.
- `main.rs`'s boot sequence ordering (`init_kernel_stacks` →
  `scheduler::init` → `init_trap_frames` → `init_process_slots` → ...)
  has a hard invariant (everything guard-paged must exist before the
  first `AddressSpace::new()`) enforced entirely by comments, not by
  the type system. Confirm the ordering is still correct and that any
  future insertion point is obvious to whoever adds the next one.
- `kernel/src/task/scheduler.rs`'s `map_guarded`/`map_guarded_raw`
  split (introduced for `ProcessBox`) — confirm both call sites
  (`Inner`'s fields, `trap_frame`, `ProcessBox`'s slot pool) still
  agree on the "guard page above and below" contract.

**Exit condition:** every file in Scope has been read this milestone,
with any finding either fixed (small diff) or filed as its own ADR/task
if it's genuinely bigger than a base-level check should absorb.

### Phase 2 — Lint and dead-code cleanup

`cargo run -p xtask -- build` currently produces 8 warnings:

- `memory/mod.rs:16` — unused imports (`Page`, `PageTableFlags`,
  `PhysFrame`, `Size4KiB`)
- `arch/x86_64/lapic.rs:79` — `REG_ID` never used
- `arch/x86_64/lapic.rs:286` — `this_lapic_id` never used
- `arch/x86_64/percpu.rs:39` — `PerCpuSlot.spin_count` never read
- `driver/mod.rs:20` — `Driver::name` never used
- `memory/virt.rs:119` — `unmap` never used
- `memory/virt.rs:128` — `translate` never used
- `task/process.rs:330` — `Process::new_dummy` never used (only true
  outside the `kitchen-sink-test`/dummy-process features — confirm
  that's the actual reason before touching it)

For each: either it's genuinely dead (delete it — this project already
avoids speculative code, per its own stated style) or it exists for a
specific future consumer (a driver that will read the LAPIC ID, a
`Driver` trait method a real driver framework will use) — in which
case say so in a doc comment and `#[allow]` it explicitly, rather than
leaving an unexplained warning for the next contributor to wonder
about.

Separately: `cargo clippy -p tarnos-kcore -p tarnos-abi --all-targets --
-D warnings` is the only clippy CI runs. The kernel crate has never
been linted — running it this session surfaced 13 findings (one real
style fix, `n.is_multiple_of(...)`; the rest — `needless_range_loop` on
loops like `scheduler.rs`'s `for core in 0..MAX_CORES`, `type_complexity`
on `driver::IRQ_TABLE`'s `SpinLock<[Option<fn()>; MAX_IRQ]>` — are
worth a deliberate decision each, not a reflexive fix: several of these
loop variables (`core`, `index`) are used for more than indexing
(printed in diagnostics, passed to other calls), where clippy's
suggested `.iter().enumerate()` rewrite would be a strict readability
regression). Decide per-lint whether to fix, or to `#![allow]` at the
crate level with a comment explaining why (matching how deliberately
this codebase already justifies every other departure from a "just
follow the tool" default). Extend CI's clippy step to cover
`tarnos-kernel`, `tarnos-rt`, and every userland crate once this pass
is clean, so this gap doesn't silently reopen.

**Exit condition:** `cargo run -p xtask -- build` produces zero
warnings (fixed, or justified with an `#[allow]` and a comment); clippy
runs clean (fixed or justified the same way) against every crate in
the workspace, kernel included; CI's clippy step covers all of them.

### Phase 3 — Unsafe-code audit

225 `unsafe` occurrences in `kernel/src`, 16 in `libs`, 0 in
`userland`. Every one should already carry a `# Safety` doc comment
(this project's own established convention — spot-check that this is
actually still universal, not just true of the oldest code) explaining
which invariants the caller must uphold and why they hold at every real
call site. For each `unsafe fn`/`unsafe {}` block:

- Confirm the safety comment is still accurate — several exist for
  invariants established *before* this session's own guard-paging
  refactors (trap frames, `ProcessBox`) and should be re-checked
  against the current memory layout, not just re-read for prose
  quality.
- Confirm there's no `unsafe` block wider than it needs to be — a
  common drift pattern is a safety-relevant operation gaining
  unrelated, safe neighbor code inside the same block over several
  edits.
- Spot-check the raw `global_asm!`/`asm!` blocks in `context_switch.rs`,
  `smp.rs`, `lapic.rs`, `percpu.rs`, `syscall.rs` specifically — these
  are the highest-consequence unsafe code in the kernel, already
  audited multiple times across this investigation's own ADRs, and
  worth one more confirming pass given how central they are to
  anything a filesystem or driver will eventually call through.

**Exit condition:** every `unsafe` block/fn has an accurate, current
`# Safety` comment; no block is wider than its actual safety-relevant
operation.

### Phase 4 — Test and CI coverage audit

24 `xtask test-*` scenarios exist, run individually in CI plus rolled
into `test-all`. For this phase:

- Confirm every scenario in `xtask/src/main.rs` is actually wired into
  both `test-all` and `.github/workflows/ci.yml` — a scenario that
  exists but isn't called from either is silently untested.
- Confirm `test-kitchen-sink`'s exclusion from `test-all`/CI (per every
  ADR since 0012) is still correctly reflected in both places, and that
  running it manually still works as its own documented entry point —
  it shouldn't bit-rot just because it's excluded from the main suite.
- Look for coverage gaps: is there a scenario for every syscall
  (`SYS_YIELD`/`SEND`/`RECV`/`EXIT`/`SPAWN`/`GRANT`/`PROCESS_START`/
  `WAIT`/`KILL`/`SBRK` — 10 total) exercising both its success and
  at least one boundary/error path? `grep` the ABI for the full list
  and check each one off against the scenario list rather than
  assuming.
- Confirm the BIOS and UEFI boot paths (`test-uefi-boot` and the
  default BIOS path every other scenario uses) are both still
  exercised — a regression only visible under one firmware path is
  exactly the kind of thing a "final check" exists to catch before
  something as boot-path-sensitive as a filesystem driver gets added.

**Exit condition:** `cargo run -p xtask -- test-all` green; every
scenario confirmed wired into CI; a written checklist (in this doc or
a follow-up commit) mapping each syscall to the scenario(s) that
exercise it, with any real gap either filled or explicitly deferred
with a reason.

#### Findings

Every one of the 22 non-kitchen-sink scenarios in `xtask/src/main.rs`
is wired into both `test-all` and `.github/workflows/ci.yml`'s own
step list, confirmed by direct comparison — no orphaned scenario
exists in either direction. `test-kitchen-sink` is correctly excluded
from both, still runs cleanly as its own manual entry point (confirmed
via a direct `cargo run -p xtask -- test-kitchen-sink` this pass —
`KS_IPC__OK`/`KS_HEAP_OK`/`KS_LIFE_OK`/`KS_KILL_OK`, clean halt), and
both the BIOS (every scenario's own default) and UEFI
(`test-uefi-boot`) boot paths are exercised.

Syscall coverage checklist — success path / boundary-or-error path per
syscall, checked against the actual scenario code rather than assumed:

| Syscall | Success path | Boundary/error path |
|---|---|---|
| `SYS_YIELD` | Nearly every scenario (every dummy process's own loop) | N/A — `SYS_YIELD` has no `SyscallError` variant; it cannot fail |
| `SYS_SEND` | `test-blocking-ipc`, `test-double-send`, `test-spawn-ipc`, `test-smp-send-cross-core`, `test-kitchen-sink` | **Gap** — no scenario drives `BadCapability` or `ResourceExhausted` (`QueueFull`) through an actual `sys_send` call |
| `SYS_RECV` | `test-blocking-ipc`, `test-spawn-ipc`, `test-smp-send-cross-core` | **Gap** — same two `SyscallError` variants as `SYS_SEND`, same gap |
| `SYS_EXIT` | Every scenario (every process's own clean exit) | N/A — `SYS_EXIT` has no `SyscallError` variant; it cannot fail |
| `SYS_SPAWN` | `test-spawn-ipc`, `test-spawn-boundary`, `test-process-lifecycle`, `test-wait-exit-code`, most others | **Gap** — no scenario spawns an unknown program name to drive `NoSuchProgram` |
| `SYS_GRANT` | `test-spawn-ipc`, `test-spawn-boundary` (check 4a) | Covered — `test-spawn-boundary` checks 1/3 drive `InvalidTarget` (non-child target) and `PermissionDenied` (rights amplification) |
| `SYS_PROCESS_START` | `test-spawn-ipc`, `test-spawn-boundary` (check 4b), most others | **Gap** — `test-spawn-boundary` only exercises the success path for this syscall; nothing drives its own `InvalidTarget` rejection (a non-child, or a child no longer `Suspended`) |
| `SYS_WAIT` | `test-wait-exit-code`, `test-smp-wait-cross-core`, `test-kitchen-sink` | **Gap** — nothing calls `SYS_WAIT` on a non-child to drive its `InvalidTarget` rejection |
| `SYS_KILL` | `test-process-lifecycle`, `test-kill-boundary`, `test-smp-kill-cross-core`, `test-smp-forced-preempt`, `test-kitchen-sink` | Covered — `test-kill-boundary` check 1 drives `InvalidTarget` (non-child target) |
| `SYS_SBRK` | `test-heap-growth`, `test-sbrk-boundary` (checks 2/4) | Covered — `test-sbrk-boundary` checks 1/3 drive `InvalidArgument` (absurd increment, negative increment) |

Five real gaps, all deliberately **deferred rather than filled this
pass**, for the same reason in each case: closing them means hand-
editing more of the delicate, register-exact raw-`asm!` dummy-process
bodies this project's own test infrastructure is built from (see
`milestone3_tests.rs`/`milestone4_tests.rs`'s own doc comments on how
easy a subtle mistake there is to introduce and how hard to debug), and
every one of the five is lower-risk than it looks, not a silent hole:

- `SYS_SEND`/`SYS_RECV`'s `BadCapability`/`QueueFull` paths run through
  exactly the same `ipc::Endpoint`/`CapTable` logic already covered
  end-to-end by `tarnos-kcore`'s own proptest suite (`libs/tarnos-kcore/
  src/endpoint.rs`'s `matches_vecdeque_model`-style tests directly
  exercise `SendOutcome::QueueFull`/`RecvOutcome::QueueFull`; `captable.rs`'s
  own proptests cover lookup-miss/wrong-rights). The syscall layer
  (`arch::x86_64::syscall::resolve_endpoint`/`sys_send`/`sys_recv`) is a
  thin, direct pass-through with no extra logic of its own to miss.
- `SYS_SPAWN`'s `NoSuchProgram` is a single `Option::ok_or` one line
  into `sys_spawn` (`crate::task::process::lookup_spawnable_module(name)
  .ok_or(SyscallError::NoSuchProgram)`) — about as low-risk a line as
  exists in this file.
- `SYS_PROCESS_START`/`SYS_WAIT`'s own `InvalidTarget` checks
  (`start_child`/`wait_for_child` in `task/scheduler.rs`) are the exact
  same ownership-check *shape* `SYS_GRANT`/`SYS_KILL` already prove
  works via `test-spawn-boundary`/`test-kill-boundary` — same
  `process.parent != Some(caller)` pattern, same bystander-`Pid` test
  trick already established by both those scenarios.

Recommendation for whoever picks this up (explicitly not required
before filesystem/driver work starts): extend `test-spawn-boundary`'s
`boundary_test_process` with one more check (`SYS_PROCESS_START` against
the same bystander `Pid` check 1 already sets up) and `test-wait-exit-code`'s
`wait_test_process` similarly, mirroring `test-kill-boundary`'s existing
bystander trick exactly — both are additive, few-line changes to
already-well-understood test bodies, not new scenarios. The
`BadCapability`/`QueueFull`/`NoSuchProgram` gaps are lowest priority of
the five, given the unit-level coverage already in place.

### Phase 5 — Documentation consistency pass

- Read all 29 ADRs once, end to end, checking each one's own claims
  against the current code — not just the two or three most recent
  ones already fresh from this session. An ADR describing a since-
  superseded design (e.g., anything describing `Box<Process>` before
  ADR 0027's `ProcessBox`) doesn't need rewriting — ADRs are a
  historical record — but should read as history, not as an implicit
  claim about current behavior; add a short "superseded by ADR 00NN"
  note wherever that isn't already obvious from context.
- There is currently no top-level README or project overview outside
  the ADRs themselves. Decide whether Milestone 10 is the point to add
  one (a short "what this is, how to build it, how to run the tests,
  where the known issues are" entry point) — reasonable either way, but
  worth a deliberate decision rather than continuing to have none by
  default.
- Confirm every doc comment that names a specific line number, address,
  or constant (this codebase does this often and well — e.g., stride
  and base-address constants across the guard-paged regions) still
  matches reality after this session's own edits.

**Exit condition:** every ADR's own claims verified against current
code (superseded ones marked as such); a deliberate yes/no on a
top-level README, acted on either way.

### Phase 6 — Known-bug containment and disclosure

The cross-core corruption bug (ADRs 0012-0029) needs one clear,
findable summary before more is built on top of the affected code —
not a fix (out of scope here, see Non-goals), a **disclosure**: what it
is, exactly when it does and doesn't apply, and what a future
contributor building the filesystem or a driver needs to know.

Write this as a short, dedicated document (or a clearly-marked section
at the top of a new README if Phase 5 adds one) stating plainly:

- The bug requires **multiple cores** and **sustained cross-core
  scheduling activity**; every `-smp 1` run across this entire
  investigation (20+ runs) has passed. Single-core builds are
  unaffected by everything measured so far.
- On multiple cores under heavy scheduling load, it's a real,
  measured-rate race (roughly 50-90% under the adversarial
  `kitchen-sink-test` stress feature at `-smp 4`, depending on exact
  QEMU acceleration settings) — not something that fires on ordinary,
  light multi-core use with the same certainty, but the same code path
  everything runs through.
- It fails safely: a kernel panic into a controlled halt, never silent
  corruption that keeps running.
- `test-kitchen-sink` is deliberately excluded from normal builds/CI
  specifically because it's the reproduction, not a real workload.
- Where the live investigation currently stands (ADR 0029) and what the
  next concrete step is, so it's pick-up-able without re-deriving this
  session's own reasoning from scratch.

**Exit condition:** one document a new contributor (or a future session)
can read in under five minutes and know exactly what the risk is, when
it applies, and where the investigation left off.

### Phase 7 — Interface readiness for what's next

Before filesystem/driver work starts, confirm the primitives it will
actually need already exist and are stable, or explicitly aren't there
yet:

- A filesystem needs, at minimum, a way to register a block-capable
  driver and a way for userland to reach it — check `ipc`/`CapTable`'s
  current shape can express "grant a process a capability to a device
  endpoint" cleanly, since that's the same shape a filesystem server
  will need for its own clients.
- `driver/mod.rs`'s current `Driver` trait (the one with the currently-
  unused `name()` method from Phase 2) was built for the UART alone —
  confirm whether it's actually general enough for a block device, or
  whether it's UART-shaped in ways that would need revisiting anyway.
  Better to notice that now than mid-driver.
- Check `elf.rs`'s loader and `tarnos-abi`'s `SyscallError` set for
  anything a filesystem-backed `SYS_SPAWN` (loading a program from a
  real filesystem instead of a boot module) would need that doesn't
  exist yet — not to build it now, just to confirm the gap is known and
  sized before it's blocking.

**Exit condition:** a short written list (this doc or a follow-up) of
what's confirmed ready, what's confirmed missing, and what's genuinely
uncertain — the actual input the next milestone's own planning needs.

#### Findings

**Confirmed ready:**

- `elf.rs`'s loader is already fully data-source-agnostic: `load(data:
  &[u8], ...)` never assumes its input came from a Limine boot module —
  it copies everything it needs out of `data` before returning, holding
  no reference to it afterward. A filesystem-backed `SYS_SPAWN` reading
  a file into a heap-allocated buffer first, then calling this exact
  same function, needs zero changes here. `ElfError`'s existing variant
  set (bad magic, wrong class, segment-bounds violations, OOM, map
  failure) already covers everything a real file's malformed contents
  could produce, independent of where the bytes came from.
- `ipc::CapTable`/`CapabilitySlot`/`Rights`'s existing grant mechanism
  (`SYS_GRANT`, narrowing rights, parent-to-`Suspended`-child only) is
  sound and directly reusable for propagating a future filesystem-
  server capability down the process tree — matching, not fighting,
  this project's own established no-ambient-authority model
  (`docs/adr/0001`/`0006`): a client process reaches the filesystem
  server because *whoever spawned it* held and passed down that
  capability, the same way `init` already grants `echo-child` its own
  `CHILD_LINK_CAP` today. This requires every future spawner to know
  which service capabilities a program needs and to hold them itself —
  a real constraint, but the deliberate one this capability model
  already commits to, not a gap.

**Confirmed missing:**

- `KernelObjectRef` has exactly one variant (`Endpoint`) — a capability
  naming a device or an open file needs its own new variant, exactly
  the extension point `docs/adr/0001` designed it to have but never
  built. `Rights` likewise has exactly two bits (`SEND`, `RECV`); a
  device/file capability will need its own rights vocabulary (read,
  write, whatever "admin" a block device needs), not a reuse of IPC's.
- `driver::Driver`/`CharDevice` are UART-shaped, confirming the
  suspicion Phase 2 already flagged: byte-oriented
  (`write_byte`/`try_read_byte`), no block/sector addressing, no
  notion of an in-flight request or completion, no I/O error type. None
  of this is reusable for a block device as-is; a `BlockDevice`-shaped
  trait (addressed reads/writes, some async-or-callback completion
  model, its own error type) needs designing from scratch, not
  extending from `CharDevice`. `Driver::name()` — the one method the
  base trait actually has — is the only part that would carry over
  unchanged.
- `tarnos_abi::SyscallError`'s 9 variants have no I/O-error shape (a
  file genuinely not existing, distinct from `NoSuchProgram`'s
  boot-module-specific meaning; permission denied opening a file; a
  read/write I/O failure). Cheap to add when needed — this is exactly
  the kind of extensible, negative-`i64`-per-variant enum `docs/adr/0004`
  designed for exactly this — but not present today; sizing this gap
  now (rather than discovering it mid-filesystem-milestone) is the
  point of naming it here.

**Genuinely uncertain — needs a decision, not more auditing:**

- Whether the near-term plan keeps drivers kernel-resident (only the
  filesystem *logic* becomes a service on top of an in-kernel block
  driver) or moves drivers out-of-process too changes the size of the
  gap enormously. If drivers stay in-kernel, the missing pieces above
  are the whole story. If they move out-of-process, there is currently
  **no mechanism at all** for a process to receive MMIO/port-I/O access
  or hardware IRQ delivery via IPC — `driver/mod.rs`'s own module doc
  comment already anticipates this ("the seed of a future out-of-process
  driver manager"), but nothing beyond the in-kernel IRQ dispatch table
  itself exists toward it. This is a materially bigger primitive than a
  new `KernelObjectRef` variant, and the next milestone's own planning
  should decide which shape it's actually building before estimating
  scope.
- Whether a filesystem server needs to hand a capability to an
  already-running, unrelated client process (per-request, e.g. "here is
  a capability naming this one open file") — which the current
  parent-to-`Suspended`-child-only `SYS_GRANT` genuinely cannot express
  — or whether the existing spawn-time propagation model (above) is
  judged sufficient for the whole milestone's scope, is a concrete
  design question for that milestone's own planning, not something
  this audit can resolve by itself.

### Phase 8 — Housekeeping

- Task #100 ("single-step capture the SpinLock<ChainedPics> null-write")
  has been pending since early in the Milestone 9 corruption
  investigation and predates most of ADRs 0013 onward, which took the
  investigation in a different direction. Read it against everything
  ADRs 0012-0029 have since found, and either close it as superseded
  (most likely) or fold whatever's still relevant into the live
  corruption-investigation task.
- Sweep the task list generally for anything else stale in the same
  way.

**Exit condition:** task list has no orphaned items whose relevance
hasn't been explicitly re-confirmed or retired.

### Phase 9 — Final sign-off

- Full regression: `cargo run -p xtask -- test-all`, green, from a
  clean `build/` (not an incrementally-reused one) at least once.
- A fresh-clone build check: `build/` and `target/` removed entirely,
  rebuilt from scratch, to catch anything that only works because of
  stale build-cache state.
- Both BIOS and UEFI boot paths verified once more, explicitly, at the
  end (not just as part of Phase 4's own audit) — the actual "yes, this
  boots cleanly, right now, on both paths" confirmation this milestone
  exists to produce.
- Write one final ADR (the next number after whatever this milestone's
  own work reaches) summarizing what Milestone 10 found and fixed —
  matching this project's own established pattern of closing every
  milestone with a written record.

**Exit condition:** all of the above green, in one sitting, on the
actual commit this milestone ends on — the base this milestone
certifies is exactly the base the filesystem milestone starts from.

## Definition of Done

Milestone 10 is complete when every phase's own exit condition is met.
Concretely: zero build warnings, clean clippy across the whole
workspace (or explicitly justified exceptions), every `unsafe` block
carrying an accurate safety comment, `test-all` green from a clean
build on both firmware paths, every ADR's claims verified against
current code, the corruption bug's risk boundary documented in one
findable place, the task list free of stale items, and a closing ADR
recording all of it.

## What happens after

Filesystem and driver work starts from this verified base. Nothing in
this milestone should need revisiting once the next one begins — that's
the point of doing it now rather than discovering the same gaps three
milestones deep into filesystem code, where fixing them costs more.
