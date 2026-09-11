# 0008: Userland Heap and SYS_SBRK

## Status

Accepted. Implemented across `kernel/src/arch/x86_64/syscall.rs`,
`kernel/src/task/process.rs`, `kernel/src/elf.rs`,
`libs/tarnos-abi/src/lib.rs`, `libs/tarnos-rt/src/{heap,syscall}.rs`, and
`userland/heap-child`.

## Context

Milestones 2-4 hardened process isolation, dynamic process creation, and
process lifecycle/termination. With those in place, the next real gap
turned out to be more basic than the previously-planned multi-core
milestone: no userland program had any way to get dynamic memory.
Confirmed directly, not assumed: there was no `mmap`/`brk`/`sbrk`
syscall anywhere in `tarnos-abi` or the kernel's dispatch table, and
`libs/tarnos-rt` had no `#[global_allocator]` — a userland binary that
tried to use `alloc::vec::Vec`/`Box` would fail to link. This blocked
writing any real program (a future shell needs variable-length input
handling; almost anything beyond a fixed-size demo needs `Vec`/`Box`),
whereas multi-core didn't unblock anything else actually planned next —
the reason this milestone was reordered ahead of it.

## Decision

### `SYS_SBRK`, grow-only, relative increment

Syscall number 9, `sys_sbrk(increment: i64) -> old_break | -errno`,
Unix-`sbrk`-shaped rather than an absolute `brk`. Returns the *previous*
break on success, so `sys_sbrk(0)` is a free, side-effect-free query —
it falls out of the same code path as any other grow, not a special
case. Grow-only this milestone: a negative `increment` returns the new
`SyscallError::InvalidArgument` (value 8) rather than actually
shrinking. The ABI still reserves the negative path — same syscall
number, same calling convention — so real shrink can be added later
without breaking anything already built against this milestone.

`Process` (`kernel/src/task/process.rs`) gained `pub heap_end: u64`,
initialized to a new `pub const USER_HEAP_START: u64 = 0x4000_0000`
(1 GiB) in the shared `Process::new` constructor both `new_dummy` and
`from_elf` already build on — no duplicated init logic. A fixed
constant was chosen over computing a per-binary heap start from the
ELF's highest loaded segment: simpler, and safe today because every
current and near-term binary links far below 1 GiB.

**A pre-existing gap closed as a side effect:** `kernel/src/elf.rs` had
its own separate `USER_SPACE_MAX` constant (identical in value to
`process.rs`'s stack-top constant) used only to bound-check `PT_LOAD`
segments — meaning a malformed ELF's segment could land anywhere up to
the top of the canonical lower half, overlapping the user stack and now
the heap too. `elf.rs` now imports `process::USER_HEAP_START` in its
place; a segment must stay below 1 GiB, cleanly separating "ELF-loadable"
territory from "stack and heap" territory.

### A fixed 64 MiB heap ceiling, checked before any frame is touched

`sys_sbrk` rejects — with `InvalidArgument`, before allocating a single
frame — any increment that would push the heap past `USER_HEAP_START +
64 MiB`. Without this, a process requesting an astronomically large
(but non-overflowing) increment would have the kernel loop allocating
physical frames until all of RAM was committed to that one process's
heap before finally failing on a genuine OOM — a single-syscall,
syscall-triggerable denial-of-service against every other process on
the system. This is exactly the class of boundary bug this codebase's
hardening milestones exist to close (the same spirit as Milestone 5's
own `USER_HEAP_MAX_SIZE` check preventing what an unchecked
`allocate_frame`-until-failure loop would otherwise allow). Checked
*before* the mapping loop runs, so the adversarial case costs nothing.

Mapping happens only for the page range between the old and new break's
rounded-up page boundaries (`align_up(old_end, 4096)` to
`align_up(new_end, 4096)`), so a non-page-aligned `increment` — the
common case, since userland is free to request any byte count — never
re-maps an already-mapped page.

### The first deliberate exception to "never nest a second lock inside `with_current_process`"

`sys_sbrk`'s handler calls `p.address_space.map(...)` — which takes
`memory::phys`'s frame-allocator lock — from *inside*
`scheduler::with_current_process`'s closure, i.e. while the scheduler's
own lock is already held. Every earlier syscall handler
(`resolve_endpoint`, `sys_grant`) deliberately avoided this exact shape,
per their own doc comments: the scheduler's lock must never still be
held while code that might need to re-lock it runs. This is safe here
specifically because it was audited, not assumed: `AddressSpace::map`
builds its own per-address-space `OffsetPageTable` from `self.pml4_frame`
and only touches `GlobalFrameAllocator` — nothing in `memory::phys` or
`memory::virt` ever calls back into `task::scheduler`, so the nesting
order (`SCHEDULER` → phys-allocator lock) is strictly one-directional
with no cycle anywhere in the codebase today.

This is a precedent, not a general license: **any future syscall that
wants to nest a second lock inside `with_current_process` must repeat
this same audit** (does anything reachable from the nested lock ever
call back into `task::scheduler`?), not assume it's now generally safe
because `sys_sbrk` already does it.

### `libs/tarnos-rt/src/heap.rs`: a lazily-growing userland allocator

Mirrors `kernel/src/memory/heap.rs`'s use of
`linked_list_allocator::LockedHeap`, but grows on demand via `sys_sbrk`
instead of eagerly reserving a fixed region at boot — userland has no
boot-time knowledge of how much memory it will ever need. `MIN_GROW_BYTES
= 16 KiB` amortizes the syscall cost the same way a real libc allocator
amortizes `brk`/`mmap` calls, rather than trapping into the kernel on
every single `alloc`.

One real subtlety this surfaced:
`linked_list_allocator::Heap::extend` panics if called before the heap
has ever been initialized (`LockedHeap::empty()` starts with no backing
memory at all). `UserHeap` tracks this with an `AtomicBool`: the very
first growth calls `.init(bottom, size)`, every growth after that calls
`.extend(size)`. Defining `#[global_allocator]` inside `tarnos-rt` (a
library), rather than requiring each binary to declare its own, is
correct and matches how Rust's global allocator attribute already works
crate-graph-wide for `entry_point!` and the panic handler — every
binary that links `tarnos-rt` gets it for free.

No shrink support: `dealloc` frees back into the `linked_list_allocator`
free list for reuse within the same process, never back to the kernel —
consistent with the grow-only decision above.

### `userland/heap-child`

A dedicated new fixture, not bolted onto an existing one — matching the
established one-purpose-per-fixture convention (`echo-child` does IPC,
`exit-code-child` reports an exit code). It builds a 512 KiB `Vec<u64>`
(well past `MIN_GROW_BYTES`, forcing dozens of separate `sys_sbrk`
growths rather than one lucky allocation), verifies every value it just
wrote is still intact, and exits `0`/`1` — verified by the parent's
`SYS_WAIT`, the exact pattern `exit-code-child` established in
Milestone 4. It needs no capabilities at all, since it never does IPC.

## Consequences

- Any userland binary linking `tarnos-rt` can now use `alloc::*` types
  (`Vec`, `Box`, `String`, ...) — the concrete gap this milestone closes,
  proven end to end by `xtask test-heap-growth`.
- `xtask test-sbrk-boundary` proves the boundary is enforced, not just
  unexercised: an increment over the 64 MiB ceiling and a negative
  increment are both rejected, a valid grow succeeds, and a
  zero-increment query is genuinely side-effect-free.
- **No real shrink** — `SYS_SBRK` only grows. Revisit if a future
  program actually needs to release heap memory back to its own
  address space; the reserved negative-increment path means this
  doesn't need a new syscall number when it happens.
- **A fixed, non-configurable 64 MiB heap ceiling per process** — not a
  quota system, just a single hardcoded constant guarding against the
  single-syscall OOM-DoS described above. Revisit if a real program
  needs more, or if per-process memory accounting (already named as
  deferred in `docs/adr/0007`) is ever added.
- **`USER_HEAP_START` is a fixed constant, not derived from the ELF** —
  a future binary that somehow linked above 1 GiB would collide with
  its own heap. Nothing built so far does, or is expected to soon.
- **A new, audited exception to a previously-absolute lock-ordering
  rule.** `sys_sbrk` is the only syscall handler that nests a second
  lock inside `with_current_process`. Safe today by direct audit of
  every call path the nested lock can reach; any future syscall wanting
  to do the same must repeat that audit rather than treat this as
  precedent that it's now unconditionally safe.
- `ExitStatus`/process-lifecycle machinery from `docs/adr/0007` is
  unchanged by this milestone — `heap-child` is a completely ordinary
  process from the scheduler's point of view.
