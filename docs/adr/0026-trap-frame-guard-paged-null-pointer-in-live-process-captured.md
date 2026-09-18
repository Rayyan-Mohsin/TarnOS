# 0026: `trap_frame` Guard-Paged; a Complete Null-Pointer Capture Moves the Target One Level Deeper

## Status

Accepted. Permanent hardening added (kept regardless of outcome, same
reasoning as ADR 0025). The guard pages around `trap_frame`'s own
*target* memory came back clean a third time, but this round produced
the single most decisive, complete capture of the whole investigation:
proof that a live `Process` struct's own field, sitting in its own
heap allocation, gets overwritten with zero while the process is
still running. Root cause remains open, but the search target has
moved: from "the scheduler's shared state" to "a specific process's
own private, heap-allocated struct."

## Context

ADR 0025 guard-paged the scheduler's own `Inner` state — the process
table, generation counters, ready queue, and per-core `current[]`
array — at two granularities, both coming back clean. That result's
own "Consequences" section named the natural next target directly:
`Process::trap_frame`, which ADR 0023's double-fault capture had
already implicated (a fault immediately after resuming into
`process.trap_frame`'s own address) but which lives in a *separate*
heap allocation (`Box<Process>`), never covered by `Inner`'s guard
pages.

## Implementation

`Process::trap_frame` used to be an inline, by-value `TrapFrame` field.
It's now `&'static mut TrapFrame`, pointing into its own dedicated,
guard-paged slot — one per possible process-table index, mirroring
`init_kernel_stacks`'s existing "eager, for every possible slot, once"
pattern exactly, reusing `task::scheduler::map_guarded` (made
`pub(crate)` for this) as the underlying primitive. `init_trap_frames()`
maps every slot at boot, at the same point `init_kernel_stacks()` and
`task::scheduler::init()` already run, for the identical ordering
reason (every `AddressSpace`'s one-time kernel-half PML4 snapshot must
already include it). `Process::new` — the one path every process
construction funnels through — now writes its process's real initial
`TrapFrame` into that slot via a new `trap_frame_for(pid)` accessor,
rather than embedding the value inline.

Because `&mut [T; N]`/`&mut TrapFrame` auto-deref for indexing, method
calls, and field access exactly like the plain values they replace,
nearly every existing read/write of `process.trap_frame.<field>`
throughout `scheduler.rs` needed no change at all. Four call sites that
touched the *whole* `TrapFrame` rather than one of its fields needed an
explicit deref, caught immediately by the compiler: two that took a
reference/pointer to the whole struct (`&process.trap_frame` →
`&*process.trap_frame`; `&mut process.trap_frame as *mut TrapFrame` →
`&mut *process.trap_frame as *mut TrapFrame`), and two whole-struct
assignments (`process.trap_frame = unsafe { *current_frame }` →
`*process.trap_frame = unsafe { *current_frame }`, writing *through*
the guard-paged reference rather than rebinding it) — the same pattern
ADR 0025 already established for `sched.ready`'s own equivalent case.

## Result: a third null result for the guard, and the clearest capture yet

Full regression (22/22) and a boot smoke test passed before stress
testing, as with every prior round.

Two stress batches against the pure-yield reproduction (`-smp 4`, 17s
then 30s timeout, 40 runs each): 32/40 and 36/40 total panics. Neither
batch put a single hit inside or near the new guarded trap-frame
region (`0xffff_9750_...`) — a third clean null result for "a wild
write overshoots into a guarded region," now covering the scheduler's
whole state, its fields individually, and this per-process target too.

But the first batch caught something new at the exact line this round
added (`scheduler.rs:860`, `*process.trap_frame = unsafe { *current_frame }`):
a **misaligned pointer dereference** panic — Rust's own debug-mode
safety check firing because the *value currently stored in*
`process.trap_frame` (not the memory it points at) was not a valid
`TrapFrame` pointer. That first capture was itself truncated (cut off
mid-message with no timeout pressure at all — the panic fired less
than a second after boot, nowhere near the 17s budget — so this was
very likely the same handler genuinely stalling while formatting this
specific message, not merely running out of time). The follow-up
30s-timeout batch caught the same failure shape again, this time
complete: `scheduler.rs:860:44: null pointer dereference occurred` —
Rust's fixed-text message for exactly this case, with nothing left to
truncate.

This is decisive in a way no earlier capture was. `trap_frame_for`
only ever computes a fixed, non-zero, page-aligned address
(`TRAP_FRAME_BASE + index * TRAP_FRAME_STRIDE + 4096`) — there is no
code path that could construct or store a null reference there in the
first place. For `process.trap_frame`'s *stored value* to read back as
null later, something wrote zero bytes over that exact field, in a
live, already-constructed `Process` sitting inside its own `Box`
allocation on the general kernel heap — not a wild pointer computed
from unrelated code and landing on scheduler memory by chance (both
granularities of that hypothesis are now closed by ADR 0025), and not
a bad value merely *read out of* trap-frame memory (that memory's own
guard pages stayed clean). The corruption reaches inside a specific
process's own private struct.

## Testing

- `cargo run -p xtask -- build`: clean.
- `cargo run -p xtask -- test-fault`: clean single-panic boot smoke
  test.
- Full 22-scenario regression suite: green — including
  `fault-isolation-test`, which caught a real mistake mid-round (see
  Consequences) before this ADR's own results were trusted.
- Pure-yield reproduction, `-smp 4`: 17s timeout (32/40) and 30s
  timeout (36/40) batches, 40 runs each — no hits in the new guarded
  region either time; one truncated and one complete capture of the
  same `scheduler.rs:860` null/misaligned-pointer failure.
- Every ISO build verified via its own boot-log line and
  `strings ... | grep -c "pure-yield process"` before trusting a batch
  against it.

## Consequences

- The guard-paged `trap_frame` relocation is permanent, kept regardless
  of this round's own null result on its primary question — same
  reasoning as ADR 0025's guarded `Inner` fields.
- **Process-lifecycle note, caught mid-round**: reverting this round's
  temporary pure-yield `main.rs` experiment via a bare
  `git checkout -- kernel/src/main.rs` silently discarded this round's
  own uncommitted `task::process::init_trap_frames()` call alongside
  it (identical to a mistake already made and fixed in ADR 0025's own
  round) — `git checkout --` reverts a file to its last *commit*, not
  to "everything except the temporary part," and any other permanent,
  not-yet-committed change to that same file is not exempt. Caught both
  times by the standing discipline of re-running the full regression
  suite immediately before every commit, never skipped even under time
  pressure; `fault-isolation-test` failed outright both times with an
  unmistakable `task::scheduler::init() must run before the scheduler
  is used`-shaped panic. Worth calling out a second time as a durable
  process note: commit a permanent `main.rs` change *before* applying
  a temporary one on top of it, or restore the temporary block by hand
  (`Edit`, not `git checkout`) instead of reverting the whole file.
- The next productive step is now specific and well-motivated: find
  what can write zero bytes into a live `Process` struct's own field
  inside its `Box` allocation. Candidates worth checking directly:
  whether anything ever obtains a *second*, aliasing `&mut Process` (or
  a raw pointer to one) for the same table slot while another is still
  live; whether `Box<Process>`'s own allocation could ever be
  mistaken for freed/reusable memory while still referenced from
  `Slot::Occupied`; and whether the general heap allocator itself
  (`linked_list_allocator`, unlike every guard-paged region in this and
  ADR 0025, still ordinary, unguarded `.bss`-backed memory) has a bug
  that hands out overlapping allocations under adversarial concurrent
  pressure. Guard-paging `Box<Process>`'s *entire* allocation (not just
  one field) is the natural fourth experiment if a static read of those
  candidates doesn't turn up the mechanism directly.
- `test-kitchen-sink` stays out of `test-all`/CI. Root cause of the
  underlying corruption remains open.
