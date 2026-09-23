# 0032: Milestone 12 — A Minimal Read-Only Filesystem

## Status

Accepted. Implemented across `kernel/src/fs/{mod,fat}.rs`,
`kernel/src/{arch/x86_64/syscall,ipc/capability,main,milestone12_tests}.rs`,
`libs/tarnos-abi/src/lib.rs`, `libs/tarnos-rt/src/syscall.rs`,
`targets/x86_64-tarnos-user.json`, `userland/init/src/main.rs`, a new
`userland/fs-child` crate, `xtask/src/main.rs`,
`.github/workflows/ci.yml`, `limine.conf`, the root `Cargo.toml`, and
`docs/KNOWN-ISSUES.md`. `docs/MILESTONE-12-FILESYSTEM.md` records the
full plan and every phase's own detailed findings; this ADR is its
closing summary, matching every prior milestone's own pattern
(most recently `docs/adr/0031`).

## Context

Milestone 11 built a proven, tested block driver — a process can issue
a capability-gated syscall and read raw sectors, by LBA, off a real
virtio-blk device — but deliberately stopped there: this kernel still
had no notion of a "file." `SYS_SPAWN` could only create a process from
Limine's own fixed boot-module set; nothing could read a named file's
content off the disk Milestone 11 proved it could talk to. Milestone
11's own closing ADR named this milestone directly and left one loose
end for it to pick up on purpose: `tarnos_abi::BLOCK_CAP` was seeded
directly into a throwaway boot-time fixture for Milestone 11's own
testing, never into the real `init` process's own capability table,
because nothing yet needed `init` to hold it. This milestone was that
need.

Carried forward unchanged from Milestone 10's Phase 7 and Milestone
11's own Decisions: this filesystem stays kernel-resident, and it reads
through Milestone 11's own `driver::block::BlockDevice` trait, not
around it.

## Decision

### Phase 1 — Research and confirm the actual environment

Confirmed FAT12 against a real `mformat`/`mcopy`-built 1.44 MiB image,
not assumed from the spec: every BPB field, the spec's own
authoritative cluster-count rule for FAT type (never the informational
`FilSysType` label string), FAT12's packed 12-bit cluster-chain
encoding (confirmed against both a single-cluster and a real
multi-cluster chain), the 32-byte directory-entry layout, and the
cluster-to-LBA mapping formula — all read directly off real bytes via
`od`, matching Milestone 11's own Phase 1 discipline for the exact
virtio-blk device variant. Confirmed the two on-disk FAT copies are
byte-identical, informing the Decision that the parser reads only the
first.

### Phase 2 — FAT parsing core

Implemented `fs::fat::Fat12Volume` (`mount`/`read_file`): BPB parsing
with the same authoritative cluster-count check, FAT12 cluster-chain
traversal bounded against the volume's own total cluster count (so a
cyclic or corrupt chain can never loop forever), flat root-directory
8.3 lookup, and a full-file read into a kernel-owned `Vec<u8>` — built
entirely on `driver::block::BlockDevice`, doing its own LBA math and
never reaching into `virtio_blk`'s own internals. Caught one real bug
on the first boot attempt: reading a whole multi-sector FAT table in
one `BlockDevice::read_sectors` call exceeded `virtio_blk`'s own
per-request limit; fixed by reading one sector at a time, a property of
`fs::fat`'s own reliance on `BlockDevice`'s minimum contract rather
than a driver-specific constant. The kernel-internal smoke test checks
both a single-cluster and a multi-cluster file, so it actually
exercises a real chain walk.

### Phase 3 — Capability and syscall surface

Added `KernelObjectRef::FsRoot` (a pure marker, reused `Rights::READ`
rather than a new bit — the object-variant match already distinguishes
it from `BlockDevice`) and `tarnos_abi::FS_CAP`. Implemented
`SYS_FILE_READ` — a length-bounded prefix read (`min(file_size,
buf_len)` bytes, since `fs::fat::read_file` itself has no partial-file
read), packing its file name via the exact same fixed-width,
pointer-free scheme `SYS_SPAWN` already used for a program name (that
helper was renamed from `pack_program_name` to the generic
`pack_short_name` once a second real caller needed it). The destination
buffer's validation and copy logic (`validate_write_range`/
`copy_into_user_range`) was extracted out of `sys_block_read` for the
same reason. `fs::fat::mount_root` runs unconditionally in the real
boot sequence, right before `init` is spawned; `FS_CAP` is seeded into
`init`'s own table only when it actually finds a valid FAT12 volume —
closing `BLOCK_CAP`'s own deferred loose end for real, not through a
throwaway fixture, and fixing a stale doc comment that had wrongly
claimed this already happened for `BLOCK_CAP` itself. `init` probes it
unconditionally, tolerant of absence.

Root-caused a real, previously-latent bug along the way: `targets/x86_64-tarnos-user.json`
never disabled SSE the way the kernel's own built-in target does, so
`init`'s first array-sized zero-init/comparison code (nothing before
this milestone was large enough to trigger it) got auto-vectorized into
SSE instructions — undefined on a kernel that never sets up
`FXSAVE`/`XSAVE` or `CR4.OSFXSR`, causing a real ring-3 invalid-opcode
fault. Fixed by copying the kernel target's own `features`/`rustc-abi`
settings; confirmed via `objdump` that the rebuilt binary carries zero
SSE instructions.

### Phase 4 — `SYS_SPAWN` filesystem fallback

`sys_spawn` now falls back to `fs::fat::with_root`'s `read_file` when
the boot-module registry misses, before failing with `NoSuchProgram`;
every distinct `fs::fat::FatError` collapses to that one code, matching
what a caller already handles for the boot-module-miss case. `init`
unconditionally probes this too (`SYS_SPAWN("FSCHILD.ELF")`, a name
absent from `limine.conf`), reusing the already-proven
`exit-code-child` binary as the fixture rather than a new crate.

Found and fixed a second real, previously-latent bug: `tarnos_rt::syscall::sys_wait`
read `kind`/`code` from `rsi`/`rdx`, but the kernel always writes
`kind` into `rdi` and `code` into `rsi`. This exact wrapper had existed
since Milestone 4 and was never once exercised — every existing
`SYS_WAIT` test uses a raw-`asm!` dummy process reading the registers
directly and correctly; `init`'s own new probe is the first real,
compiled userland code in the project ever to call `sys_wait` through
its own safe wrapper. Isolated first against a plain boot-module
target to rule out anything spawn-fallback-specific before fixing
`tarnos-rt` itself.

### Phase 5 — Userland fixture and adversarial testing

Added `userland/fs-child` (mirroring `block-child`'s own Milestone 11
shape exactly: a real, independently-linked ELF fixture, spawned
directly with `FS_CAP`/`CONSOLE_CAP` seeded into its own table) and
`milestone12_tests::fat_boundary_process` (a dummy process running four
adversarial `SYS_FILE_READ` probes: a missing file, an unseeded
capability index, an unmapped destination buffer, and a buffer larger
than the file's own size — which must succeed, truncated to the file's
exact real length, proving the length-bounded-prefix-read semantics are
real). Two new `xtask` scenarios, both passing on their first real boot
attempt once every register/name-packing constant was independently
verified against a short script rather than hand-computed alone, given
this milestone's own now-twice-recorded history of exactly this kind
of register-mapping bug slipping past casual inspection.

## Findings worth recording independent of the plan

- **Two real, previously-latent bugs, both found by the same pattern:
  the first real exercise of an existing code path.** The userland
  target's missing SSE-disable (Phase 3) and `sys_wait`'s
  register-mapping bug (Phase 4) had both existed since early
  milestones, invisible because nothing before this milestone's own new
  code (a real compiled process doing sizable array operations; a real
  compiled process calling `sys_wait` through its own safe wrapper
  rather than raw `asm!`) ever actually exercised them. Both were
  root-caused methodically rather than worked around locally: the SSE
  bug via `objdump` disassembly and comparing the two targets' own full
  spec JSON side by side; the `sys_wait` bug via temporary kernel-side
  tracing that first proved the kernel itself was computing the right
  answer, then isolating the userland-side bug against a plain
  boot-module spawn (no filesystem fallback involved at all) before
  touching `tarnos-rt`. Both fixes are structural (a target-spec field,
  a register-mapping correction), not point patches around this
  milestone's own new call sites — every future userland crate benefits
  from both, not just this one's.
- **A disk-image-size lesson.** A debug ELF binary's own DWARF sections
  (`.debug_info`, `.debug_str`, ...) carry no `SHF_ALLOC` flag and are
  never read by `Process::from_elf`'s PT_LOAD-segment loader, but they
  are real bytes on disk — `exit-code-child`'s own ~2.9 MiB debug build
  didn't fit a 1.44 MiB floppy at all. `strip` (784 bytes afterward) is
  the smaller, more direct fix than a second release-profile build.

## Consequences

- A process can read a named file's content off a real FAT-formatted
  disk through a capability-gated syscall and get back correct bytes,
  and `SYS_SPAWN` can load and run a program that exists only on that
  disk — both proven on a completely ordinary real boot (`init`'s own
  unconditional, tolerant-of-absence probes), not only through
  dedicated test fixtures.
- `BLOCK_CAP`'s own long-deferred loose end (Milestone 11's Consequences)
  is closed for real: `FS_CAP` is genuinely wired into the real `init`
  process's own capability table at boot, not a throwaway fixture.
- Two real, previously-latent bugs affecting every userland crate
  (missing SSE-disable in the shared target spec; a swapped register
  mapping in `tarnos_rt::syscall::sys_wait`) are fixed permanently, not
  worked around locally — found only because this milestone's own new
  code was the first to actually exercise either path.
- Read-only, 8.3-only, flat-root-directory-only, no caching, no
  out-of-process filesystem server — every one of this milestone's own
  stated Non-goals holds exactly as scoped; none were quietly expanded
  or discovered to be insufficient during implementation.
- Full regression (`test-all`, 30/30 — the pre-existing 26 plus this
  milestone's four) is green from a clean, from-scratch rebuild;
  `test-smp-sched-concurrency`'s own pre-existing, environment-specific
  timing sensitivity (confirmed via `git stash` comparison against the
  unmodified base, not assumed) surfaced repeatedly during this
  milestone's own verification but is unrelated to any change here. The
  cross-core scheduling corruption bug (`docs/adr/0012`-`0029`) remains
  open, unaffected, and still correctly excluded from `test-all`/CI via
  `test-kitchen-sink`'s own standalone command, confirmed still
  runnable (reproducing the same, already-documented panic signature).
  Both boot paths (BIOS and UEFI) verified in the same clean run.
  `cargo clippy` clean across the entire workspace — every kernel
  feature configuration, `tarnos-rt`, every userland crate including
  the two new fixtures (`fs-child`, and `FSCHILD.ELF`'s reuse of
  `exit-code-child`), and both host-buildable crates (45 tests green).
