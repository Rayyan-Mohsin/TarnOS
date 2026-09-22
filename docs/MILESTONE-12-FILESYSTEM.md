# Milestone 12: A Minimal Read-Only Filesystem

## Why this milestone exists

Milestone 11 built a proven, tested block driver: a process can issue a
capability-gated syscall and read raw sectors, by LBA, off a real
virtio-blk device. It deliberately stopped there — this kernel still
has no notion of a "file." `SYS_SPAWN` today only creates a process
from Limine's own fixed boot-module set
(`task::process::SPAWNABLE_MODULES`, populated once from
`MODULES_REQUEST` at boot); nothing can read a named file's content off
the disk that Milestone 11 just proved it can talk to.

Milestone 11's own closing ADR (`docs/adr/0031`) named this milestone
directly ("What happens after," in the milestone plan) and left one
loose end for it to pick up on purpose rather than build ahead of a
real need: `tarnos_abi::BLOCK_CAP` was seeded directly into a
throwaway boot-time fixture for Milestone 11's own testing, never into
the real `init` process's own capability table — because nothing yet
needed `init` to hold it. This milestone is that need: a real
filesystem is the first thing with an actual reason to grant
disk-reading authority from `init` down to a real consumer via
`SYS_GRANT`, the same path `CHILD_LINK_CAP`/`echo-child` already
established.

Two decisions Milestone 10's own Phase 7 and Milestone 11's Decisions
already made, carried forward unchanged: this filesystem stays
kernel-resident (no isolated filesystem-server process — that still
needs an unbuilt MMIO/IRQ-via-IPC primitive with no second consumer to
justify it), and it will read through Milestone 11's own
`driver::block::BlockDevice` trait, not around it.

## Decisions

**Format: FAT, read-only.** Simple, exhaustively documented, and any
tool on the build host can already write one — no custom on-disk format
to design or to convince a future maintainer is correct. Confirmed
directly in this environment (not assumed): `mtools` is already
installed here (`mformat`, `mcopy`, `mdir`), meaning `xtask` can build
a real, standards-compliant FAT image for every test scenario without
`dosfstools`/`mkfs.fat` or a loopback mount, matching the same
"trusted, no-sudo, host-tool-driven" build style `xtask` already uses
for the ISO itself (`xorriso`, `limine`). **FAT12 at a standard 1.44 MiB
floppy geometry** is this plan's expected choice — the smallest, most
conventional, best-tooled FAT variant, and this milestone's test images
have no reason to be larger — but Phase 1 confirms this against a real
`mformat`/`mcopy`-built image before Phase 2 starts, the same
"confirmed directly, not assumed" discipline Milestone 11's own Phase 1
applied to the exact virtio-blk device variant.

**One coarse capability, no per-open-file object.** `SYS_BLOCK_READ`
(Milestone 11) already established a pattern that worked cleanly: one
capability gates "may do this kind of I/O at all," and each syscall is
a self-contained request (path, offset, length) with no persistent
kernel-side handle to leak, exhaust, or synchronize. This milestone
reuses that shape for file reads rather than introducing a Unix-style
`open()` returning a rights-bearing file descriptor — a real, useful
future extension if concurrent multi-process file access ever needs
per-handle state, but nothing built so far needs it, and Milestone 11's
own single-request `virtio_blk` driver already established this
project's preference for the simplest correct thing over speculative
generality.

**Flat root directory only.** FAT's directory entries can point to
subdirectories, but walking a full path is real, separable complexity
this milestone's own boot-module-replacement use case (loading a named
program) doesn't need yet — every file this milestone's own tests
create lives directly in the root directory. Revisit if a real need for
nested paths appears.

## Scope

- A new `fs::fat` kernel module: boot sector/BPB parsing, FAT table
  traversal (cluster-chain following), root-directory entry parsing,
  8.3 filename lookup, and reading a named file's full contents into a
  kernel-owned buffer — built entirely on top of
  `driver::block::BlockDevice`/`driver::virtio_blk::with_device`, doing
  its own LBA math, never reaching into the driver's own internals.
- `KernelObjectRef` gains an `FsRoot`-shaped variant (or equivalent —
  exact shape decided in Phase 3, informed by what the one real syscall
  needs, the same "design after building the one real thing" discipline
  Milestone 11's own `BlockDevice` trait followed); `Rights` gains
  whatever bit a file-read capability needs; `SyscallError` gains
  filesystem-specific variants (`NoSuchFile`, at minimum).
- One new syscall reading a byte range from a named file, gated by a
  capability — this time seeded into the *real* `init` process's own
  table at boot (closing Milestone 11's own named loose end), granted
  onward to a spawned child via `SYS_GRANT` exactly the way
  `CHILD_LINK_CAP` already is.
- `SYS_SPAWN`'s existing name lookup extended: if a name isn't in the
  fixed boot-module registry, fall back to a filesystem lookup by the
  same name before failing with `NoSuchProgram` — no new syscall number,
  the existing ABI surface just becomes able to find more programs.
- `xtask` changes: build a real FAT image via `mformat`/`mcopy` instead
  of a raw pattern-seeded blob, seed it with known test files (including
  one spawnable-only-from-disk program), and new adversarial `test-*`
  scenarios: the happy-path read, a missing file, a read past a file's
  own end, a request without holding the capability, and spawning a
  program that exists only on the filesystem, not in boot modules.
- A new userland test fixture (matching `block-child`'s own shape: one
  purpose, reads a known file, reports pass/fail).

## Non-goals

- **No writes.** Create, delete, modify, truncate, or resize — none of
  it. Read-only, the same reasoning Milestone 11's own driver scope
  used: smaller surface, smaller blast radius, nothing built so far
  needs write support.
- **No long filenames (VFAT).** 8.3 names only — real, well-documented
  extra complexity (checksum-linked entries, UTF-16 name fragments)
  this milestone's own test fixtures don't need.
- **No subdirectories.** See Decisions above.
- **No general virtual-filesystem (VFS) layer.** Exactly one filesystem
  implementation over exactly one already-proven block device — the
  same "no general framework for a second thing that doesn't exist yet"
  discipline Milestone 11 applied to its own PCI enumeration and driver
  registry.
- **No caching.** Every read goes to the device fresh, matching
  Milestone 11's own "simplest correct thing first" precedent.
- **No out-of-process filesystem server.** See Decisions above.

## Phases

### Phase 1 — Research and confirm the actual environment

Confirm the exact FAT variant and construction tooling against a real,
built image in this environment — not assumed from the FAT spec alone.
Build a small 1.44 MiB image with `mformat`, copy a known test file in
with `mcopy`, and inspect its actual on-disk bytes directly (a hex dump
of the boot sector and root directory) to confirm the concrete field
layout and offsets this milestone's parser will read, the same
discipline Milestone 11's own Phase 1 applied to the virtio-blk device
before writing a line of driver code.

**Exit condition:** a short written note confirming the exact FAT
variant, sector/cluster geometry, and boot-sector/root-directory byte
layout against a real image this environment actually built — not
copied from a spec.

### Phase 2 — FAT parsing core

Implement `fs::fat`: parse the boot sector, locate and walk the FAT
table for a file's cluster chain, parse root-directory entries for 8.3
name matching, and read a named file's full contents into a
kernel-owned buffer, sector by sector through `driver::block::BlockDevice`.

**Exit condition:** a kernel-internal (no syscall yet) smoke test reads
a known test file's known content off a real FAT image and confirms it
matches exactly, on a real boot.

### Phase 3 — Capability and syscall surface

`KernelObjectRef`/`Rights`/`SyscallError` extended as scoped above. The
new file-read syscall validates its inputs (path bounds, offset/length,
destination buffer) with the same rigor `sys_block_read`'s own
buffer-validation already established. Seeded into the real `init`
process's table this time, not a throwaway dummy process — closing
Milestone 11's own deferred loose end for real.

**Exit condition:** the same content-matches smoke test as Phase 2, now
reached through a real syscall from `init` itself.

### Phase 4 — `SYS_SPAWN` filesystem fallback

Extends the existing spawn-by-name lookup to fall back to the
filesystem when the boot-module registry doesn't have a match.

**Exit condition:** a real boot spawns a program that exists only on
the FAT image, not in `limine.conf`'s own boot-module list, and it runs
to completion normally.

### Phase 5 — Userland fixture and adversarial testing

A new userland fixture (reads a known file, reports pass/fail) plus
`xtask` changes: build the real FAT image with known test data, and new
adversarial scenarios covering the happy path and at least: a missing
file, a read past a file's own end, a request without holding the
capability, and Phase 4's own spawn-from-disk case — the same "prove
the boundary is enforced" bar every prior milestone's boundary tests
already hold to.

**Exit condition:** new scenarios pass, wired into `test-all` and CI;
full existing regression suite (Milestone 11's own 26 scenarios plus
this milestone's additions) still green.

### Phase 6 — Documentation and sign-off

Write the closing ADR, confirm `test-kitchen-sink` is still correctly
excluded and still runnable standalone, and do a full clean-build
sign-off matching every prior milestone's own closing pattern.

**Exit condition:** ADR written; full regression green from a clean
build; both boot paths (BIOS/UEFI) verified.

## Definition of Done

A process can read a named file's content off a real FAT-formatted disk
through a capability-gated syscall and get back correct bytes, and
`SYS_SPAWN` can load and run a program that exists only on that disk,
not in the fixed boot-module set — proven by an adversarial test suite
covering the happy path and real boundary violations, on top of the
unmodified Milestone 11 base.

## What happens after

With a real filesystem in place, a natural next step is making it the
*primary* way processes get loaded — today's fixed boot-module set
exists only because there was no other option. A future milestone could
retire boot modules other than the kernel and `init` itself entirely,
or extend the filesystem toward genuine multi-process file sharing
(the per-open-file object this milestone deliberately deferred). Either
is real, separable work this milestone's own scope doesn't need to
decide yet.
