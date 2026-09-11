# 0006: Dynamic Process Creation and Capability Transfer

## Status

Accepted. Implemented across `kernel/src/arch/x86_64/syscall.rs`,
`kernel/src/task/{process,scheduler}.rs`, `libs/tarnos-abi/src/lib.rs`,
and `userland/{init,echo-child}`.

## Context

Milestone 2 hardened the foundation Milestone 1 built without adding
any new capability: every process that has ever existed was created by
trusted boot code, once, before the scheduler ever ran. `docs/adr/0001`
named this gap explicitly at the time — "every process's full set of
reachable objects must be decided at creation time by boot code... that
syscall is deferred, explicitly, to whenever a milestone actually needs
it" — and `task::scheduler::spawn`'s `Result`-returning signature was
deliberately shaped in Milestone 2 so that the syscall this milestone
adds would inherit a safe primitive instead of a landmine.

Milestone 3 closes that gap: a running process can now create another
process and hand it specific capabilities. This is the first milestone
that adds a new capability rather than hardening an existing one, and
it unblocks everything downstream that needs more than one
boot-choreographed process (a shell, drivers as separate processes, any
real multi-process demo).

## Decision

### Spawn-by-name against a fixed, boot-shipped set — not an arbitrary blob

There is no filesystem yet, so `SYS_SPAWN` cannot take an arbitrary ELF
image. It instead names one of a fixed set of programs shipped as
Limine boot modules alongside `init` (`task::process::init_spawnable_modules`,
populated once at boot from `MODULES_REQUEST`'s response, excluding the
module named `"init"` — that one boot code loads directly). Getting an
arbitrary in-memory ELF blob into the kernel safely needs
`MessagePayload::OutOfLine` (still unbuilt — see `docs/adr/0003`) or an
equivalent shared-memory mechanism, plus a decision about how a
receiver validates a sender-supplied physical page. Pulling that in now
would have roughly doubled this milestone's scope for a generality
nothing in it actually needs yet.

### Authority over a new child is a structural fact, not a new capability kind

The seL4-shaped answer to "how does a process safely touch *another*
process's capability table" would be a capability that names a process
(`KernelObjectRef::Process(Pid)`), checked and rights-limited the same
way an endpoint capability is. That was considered and rejected for
this milestone: it is a materially bigger diff (a new `KernelObjectRef`
variant, a new rights-like concept for "administer this process") for a
generality — arbitrary processes granting into arbitrary other
processes — this milestone's actual need doesn't call for.

What's implemented instead is narrower and reuses machinery that
already exists: `Process` gained `ProcessState::Suspended` and a
`parent: Option<Pid>` field. `SYS_SPAWN` creates a child `Suspended` —
present in the process table (so its `Pid` can never be handed to a
second, unrelated `allocate_pid()` call) but absent from the scheduler's
ready queue, invisible to preemption and switching. `SYS_GRANT` and
`SYS_PROCESS_START` are permitted against a target only while
`target.parent == Some(caller_pid) && target.state == Suspended` —
checked and acted on under one lock acquisition
(`task::scheduler::with_process`/`start_child`), so there's no window
between "checked" and "mutated." This is the seL4
create-configure-resume pattern without inventing a new object kind:
authority is a fact the scheduler already tracks, not a bit stored
anywhere new. Once `SYS_PROCESS_START` releases a child, `parent` still
names its creator for the record, but no syscall treats that as
authority afterward — the child is exactly as ordinary and independent
as any boot-created process.

`task::scheduler::with_current_process` — previously hardcoded to "the
process running right now" — generalized into `with_process(pid, f)`,
looked up via `.get_mut()` rather than direct table indexing: unlike
every previous caller, `SYS_GRANT`'s `target_pid` is attacker-controlled
input from a syscall register, and must never panic on an out-of-range
value the way internal-only call sites could safely assume. `sys_grant`
calls it twice, sequentially — once against the caller's own table to
resolve and rights-check the source capability, a second time against
the target — never nested, for the same reason `resolve_endpoint`
already documented: the scheduler's lock must never still be held while
code that might need to re-lock it (here, the second `with_process`
call) runs.

### Rights can only be narrowed on grant, never amplified

`SYS_GRANT`'s requested `rights` must be a subset of what the caller's
own source capability already holds — checked before anything is
written into the target's table. A process can hand a child less than
it has; it can never manufacture more.

### PID allocation now reuses freed slots

`allocate_pid()` was a monotonic counter that never looked at whether a
process-table slot had since emptied out — meaning the kernel could
create at most `MAX_PROCESSES` (16) processes *cumulatively, over its
entire lifetime*, not 16 at a time. Harmless through Milestone 2 (only
ever one process, `init`, was ever created after boot), but Milestone 3
is the first thing that actually creates processes at runtime, making
that ceiling real. Fixed by having `allocate_pid()` scan for the lowest
currently-empty table slot instead of incrementing a counter — small
and touches the same function this milestone was already changing.
Safe without a separate atomic reservation step: every caller (trusted
boot code, and `SYS_SPAWN`'s handler, which runs with interrupts
disabled for its entire duration — see `arch::x86_64::syscall`)
allocates and spawns in the same straight-line sequence with nothing
else able to interleave on this single core.

This reuse is only sound today because nothing can terminate a
`Blocked` or `Suspended` process from the outside — there is no
kill-arbitrary-process syscall. A `Waiter::Process(pid)` sitting in an
`ipc::Endpoint`'s queue, or a `Suspended` child waiting to be released,
can therefore never be silently invalidated out from under a reused
`Pid`. **A future milestone that adds a way to kill another process
must not break this invariant silently** — it would need to either
scrub such references at kill time or add a generation counter to `Pid`
so a stale reference can't alias a reused slot.

## Consequences

- A process can now create another process and hand it exactly the
  capabilities it chooses, closing the gap `docs/adr/0001` named. The
  new integration test `xtask test-spawn-ipc` proves the happy path:
  `init` spawns `echo-child` — a process boot code never mentions at
  all — grants it a capability it started with none of, releases it,
  and completes a genuine rendezvous with it.
- `xtask test-spawn-boundary` proves the boundary is actually enforced,
  not just unexercised, the same bar Milestone 2 held its own hardening
  claims to: a grant against a real, running process that simply isn't
  the caller's child is rejected (`InvalidTarget`); a grant requesting
  rights the caller doesn't hold is rejected (`PermissionDenied`); and a
  legitimate grant + start still succeeds in the same run.
- A `Suspended` child whose parent exits (or is killed by a fault)
  before granting it anything or releasing it is orphaned permanently —
  it occupies a process-table slot with no way for anything to ever
  reach or reap it again. This is a real, narrow gap this milestone's
  own design introduces, accepted rather than fixed: a general
  `wait()`/exit-notification mechanism (tracking exit status, deciding
  zombie-reaping policy, handling a parent dying first) is comparable in
  size to this entire milestone and isn't needed for anything built so
  far — a `SYS_RECV`-based reply, as `echo-child` already does, is
  sufficient synchronization for a parent that needs to know its child
  reached some point.
  **Update (milestone 4):** closed. See
  `docs/adr/0007-process-lifecycle-and-termination.md` — `SYS_WAIT`,
  `SYS_KILL`, and a process-table `Zombie` state now exist; an orphaned
  `Suspended` child is killed outright when its parent terminates,
  rather than left stuck forever. That ADR also revises the "confers no
  further authority" claim two paragraphs above it in this document:
  `SYS_KILL`/`SYS_WAIT` treat `parent` as authority for a child's entire
  life, not only its `Suspended` window.
- The `Rights` bitflags moved from `tarnos-kcore::captable` into
  `tarnos-abi` (re-exported from its old location unchanged), since
  `SYS_GRANT` makes it part of the wire contract between kernel and
  userland — a process must be able to express which rights it's
  requesting, the same way `Message` and `SyscallError` already are
  shared wire types.
- Multiple queued receivers per endpoint remain unsupported, unchanged
  from `docs/adr/0005`.
- `AddressSpace` still leaks its physical frames on process exit — a
  pre-existing gap since Milestone 1, unrelated to and unchanged by
  this milestone, but worth naming again now that processes are created
  and destroyed more than once per boot.
