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

#### Findings

Built a real 1.44 MiB image (`mformat -f 1440 -C -i test.img ::`),
copied in two known files (`HELLO.TXT`, 8 bytes, and `BIGFILE.TXT`,
3500 bytes — chosen specifically to span more than one cluster), and
read the raw bytes back with `od` rather than trusting `mdir`'s own
higher-level listing. Every value below is read directly off that real
image, not the FAT spec's own worked examples.

- **Boot sector (LBA 0) BPB fields, confirmed byte-for-byte**:
  `BytsPerSec=512`, `SecPerClus=1` (so cluster size == sector size,
  512 bytes), `RsvdSecCnt=1` (just the boot sector), `NumFATs=2`,
  `RootEntCnt=224`, `TotSec16=2880` (2880×512 = 1,474,560 bytes,
  matching the image's own file size exactly), `Media=0xF0`,
  `FATSz16=9`, extended boot signature `0x29` present, followed by a
  real `VolID`, `VolLab` ("NO NAME"), and `FilSysType` string
  ("FAT12   ") — confirmed present but *not* trusted as authoritative
  (see below). Signature `55 AA` present at bytes 510-511, as required.
- **FAT12 confirmed via the spec's own authoritative rule, not the
  label string**: `FilSysType` is documented as informational only —
  the real determination is cluster count. Computed from this image's
  own geometry: root directory occupies `(224×32)/512 = 14` sectors;
  data region starts at LBA `1 + (2×9) + 14 = 33`; total data sectors
  `2880 - 33 = 2847`; `2847 / SecPerClus(1) = 2847` clusters. `2847 <
  4085` → FAT12, by the spec's own threshold — happens to agree with
  the label this time, but the parser will compute this, never read
  the string.
- **FAT12's packed 12-bit entry encoding confirmed against two real
  cluster chains**, using the standard `FatOffset = N*3/2`,
  even-N-vs-odd-N masking formula: `HELLO.TXT` (8 bytes, fits in one
  cluster) starts at cluster 2 with `FAT[2] = 0xFFF` (end-of-chain) —
  confirmed by reading LBA 33 directly and finding the literal bytes
  `HELLOFAT`. `BIGFILE.TXT` (3500 bytes, needs
  `ceil(3500/512) = 7` clusters) starts at cluster 3 with the chain
  `3→4→5→6→7→8→9→(0xFFF)` — confirmed by reading LBA 34 directly and
  finding the file's own known first line. `FAT[0]`/`FAT[1]` (the two
  reserved entries) read as `0xFF0`/`0xFFF`, matching the spec exactly
  for media byte `0xF0`.
- **The two on-disk FAT copies are byte-identical** in a real
  `mformat`/`mcopy`-built image (`cmp` confirmed it directly). This
  milestone's parser will read only the first copy and never
  cross-check or repair from the second — the same "smallest correct
  thing, not defensive against a case that doesn't arise" choice this
  project already made for `virtio_blk`'s own single-request driver.
- **Root directory entry (32 bytes) layout confirmed field-by-field**
  for both files: 11-byte space-padded name with no stored dot
  (`"HELLO   TXT"`, `"BIGFILE TXT"`), attribute byte `0x20` (archive,
  the default for an ordinary file), a DOS-format creation date that
  decoded to exactly this image's own real creation date (2026-09-22,
  confirmed by decoding the bit-packed year/month/day fields by hand),
  `FstClusLO` matching each file's own known starting cluster (`2` and
  `3`), and a 4-byte little-endian `FileSize` matching each file's
  exact real size (`8` and `3500`) — no LFN (long-filename) entries
  appeared for either name, as expected for names that already fit 8.3.
- **Cluster-to-LBA mapping formula confirmed end to end**:
  `LBA = DataRegionStart(33) + (cluster - 2) × SecPerClus(1)` — verified
  by computing cluster 2's and cluster 3's own LBAs from this formula
  and finding each file's real, known content exactly there, not
  assumed from the arithmetic alone.

### Phase 2 — FAT parsing core

Implement `fs::fat`: parse the boot sector, locate and walk the FAT
table for a file's cluster chain, parse root-directory entries for 8.3
name matching, and read a named file's full contents into a
kernel-owned buffer, sector by sector through `driver::block::BlockDevice`.

**Exit condition:** a kernel-internal (no syscall yet) smoke test reads
a known test file's known content off a real FAT image and confirms it
matches exactly, on a real boot.

#### Findings

Implemented `fs::fat::Fat12Volume` (`mount`/`read_file`) exactly as
scoped: BPB parsing and the same authoritative cluster-count FAT-type
check Phase 1 confirmed by hand, `fat12_entry`/`cluster_chain` decoding
FAT12's packed 12-bit entries (bounded by the volume's own total
cluster count, so a cyclic or corrupt chain can never loop forever),
flat root-directory 8.3 lookup, and a full-file read. `#[allow(dead_code)]`
gated on `not(feature = "fat-fs-test")` at the module level, matching
`driver::block`/`driver::virtio_blk`'s own Milestone 11 Phase 3
precedent exactly — this module has no caller at all until Phase 3
wires a real syscall to it.

One real bug, caught immediately by the very first real boot attempt:
`read_fat_table`'s first draft read the whole FAT table (`fat_size_sectors`
= 9 sectors on this milestone's own test image) in a single
`BlockDevice::read_sectors` call, which `virtio_blk`'s own
`MAX_SECTORS_PER_REQUEST` (8) correctly rejected as `RequestTooLarge`.
Fixed by reading one sector at a time instead — `fs::fat` was written
to depend only on the `BlockDevice` trait's own contract (a
single-sector read is always the minimum any implementation must
support), deliberately never reaching past that trait to see a
specific driver's own internal per-request limit, so the fix is a
property of `fs::fat` itself, not a constant borrowed from
`virtio_blk`.

The kernel-internal smoke test (`fat-fs-test`) checks two files, not
one: `HELLO.TXT` (single cluster) and a 3000-byte `BIGFILE.TXT`
(6 clusters at this image's `SecPerClus = 1`), so the check actually
exercises a real multi-cluster FAT12 chain walk rather than only ever
following one entry straight to its own end-of-chain marker. Passed on
the first real boot attempt once the request-size fix above landed.

**Environment note, not a regression:** `test-smp-sched-concurrency`
(a Milestone 7 scenario, untouched by this phase) failed consistently
in this session's sandbox — every attempt observed, on both this
phase's own changes and, via a direct `git stash` comparison, on the
unmodified prior commit. Every failure showed the same shape: the two
spinning test processes hadn't finished their fixed iteration count
within the scenario's fixed 15s timeout (consistently still running
around "1300 ticks" when killed). `docs/adr/0011` already documents
this scenario as sensitive to host performance; this sandbox's own TCG
(no `/dev/kvm` here) emulation speed today is evidently slow enough to
make it fail deterministically rather than occasionally. Every other
scenario in `test-all` (all 26 pre-existing plus this phase's new
`test-fat-parsing`) was individually confirmed green in this same
session. Not otherwise investigated further -- this phase's own scope
never touches scheduling/dispatch, and the baseline comparison already
rules out a regression.

### Phase 3 — Capability and syscall surface

`KernelObjectRef`/`Rights`/`SyscallError` extended as scoped above. The
new file-read syscall validates its inputs (path bounds, offset/length,
destination buffer) with the same rigor `sys_block_read`'s own
buffer-validation already established. Seeded into the real `init`
process's table this time, not a throwaway dummy process — closing
Milestone 11's own deferred loose end for real.

**Exit condition:** the same content-matches smoke test as Phase 2, now
reached through a real syscall from `init` itself.

#### Findings

**ABI.** `SYS_FILE_READ(cap, name_lo, name_hi, name_len, buf_ptr, buf_len)`
fills all six argument registers exactly (`rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9`)
-- the file name is packed the same fixed-width, pointer-free way
`SYS_SPAWN` already packs a program name, so the two now share one
renamed, generic helper (`tarnos_abi::pack_short_name`/`unpack_short_name`/
`SHORT_NAME_MAX`, was `pack_program_name`/etc.) instead of a second
scheme. Returns the number of bytes actually copied
(`min(file_size, buf_len)`) -- a length-bounded prefix read, not a
general `pread` with an arbitrary offset, since `fs::fat::read_file`
itself has no partial-file read yet; a real future need for one is a
natural, separable extension.

**Capability shape.** `KernelObjectRef::FsRoot` is a pure marker (one
mounted volume, reached through `fs::fat::with_root`), reusing
`Rights::READ` rather than adding a new bit -- the object variant match
in `sys_file_read` already distinguishes it from `BlockDevice`, so a
second "read" bit would say nothing a new bit doesn't already.
`tarnos_abi::FS_CAP = CapIndex(3)`. `sys_block_read`'s own object match
gained an explicit `FsRoot => BadCapability` arm (it was previously
non-exhaustive only over `BlockDevice`/`Endpoint`); the page-range
validate/copy logic both syscalls need was extracted into
`validate_write_range`/`copy_into_user_range` once `sys_file_read`
became a second real caller, rather than duplicated.

**Real wiring, not a test fixture.** `fs::fat::mount_root` is called
once, unconditionally, in the real boot sequence right before `init` is
spawned (`main.rs`) -- not behind any test feature. `FS_CAP` is seeded
into `init`'s own capability table only when it succeeds; most
scenarios (no disk, or Milestone 11's own raw-pattern block-test disks)
fail it harmlessly and simply don't get the capability. `init` itself
(`userland/init`) unconditionally probes it after its existing
echo-child round trip, treating `SyscallError::BadCapability` as "no
filesystem this boot" and silently continuing -- never a failure to
report. This closes `BLOCK_CAP`'s own long-deferred loose end for real
(`docs/adr/0031`'s Consequences), corrected a stale doc comment on
`KernelObjectRef::BlockDevice` that had claimed this already happened
for it, and needed no new `xtask` scenario or kernel test feature at
all: `test-fat-parsing` (Phase 2) already boots with a real FAT12 disk
and reaches real `init`, so it was simply extended to also assert
`FS_SYSCALL_OK` from `init`'s own probe.

**A second, more consequential bug: SSE was never disabled for
userland.** The first real boot attempt hit `[fault] pid ... killed:
invalid opcode`, in genuine ring-3 `init` code -- not the dummy-process
codegen landmine Milestone 11 hit twice (`Process::new_dummy`'s
copied-pages constraint doesn't apply to a real, fully-linked ELF
process). Disassembling the faulting address showed a `movups`/`movaps`
pair: LLVM had auto-vectorized `probe_filesystem`'s own array
zero-init/equality-comparison code (the first userland code, across
every milestone so far, large or slice-comparing enough to trigger it)
into SSE instructions. The kernel's own target
(`x86_64-unknown-none`, built in) explicitly disables
`sse`/`sse2`/`avx`/etc. and sets `soft-float` for exactly this reason --
this kernel never sets up `FXSAVE`/`XSAVE` state or enables
`CR4.OSFXSR`, so *any* SSE instruction in ring 0 or ring 3 is
undefined -- but `targets/x86_64-tarnos-user.json` (the custom JSON
target every userland crate builds against) never carried the matching
`features`/`rustc-abi: "softfloat"` settings, silently relying on no
userland code ever happening to need them. Fixed by copying both
fields from the kernel target's own spec exactly (confirmed via
`rustc -Z unstable-options --print target-spec-json`, not guessed);
`objdump` confirmed zero `movups`/`movaps`/`xmm` references in the
rebuilt `init` binary afterward. This is a real, previously-latent gap
that could have silently corrupted process state on a context switch
even without ever raising a fault, had a scheduling tick landed
mid-SSE-sequence on real hardware with different luck -- worth exactly
the same prominence as Milestone 11's own dummy-process bug class, and
now closed for every userland crate at once, not worked around in this
one caller.

Full regression re-run from a clean rebuild after the target-spec
fix: `test-smp-sched-concurrency` failed deterministically in this
session's own sandbox again (same shape as Phase 2's own finding,
already confirmed there via `git stash` to be pre-existing and
environment-specific, not a regression); every other scenario in
`test-all` (26 pre-existing plus `test-fat-parsing`) individually
confirmed green, including every block-test scenario -- proving the
SSE fix didn't perturb any already-working userland crate's behavior.
`cargo test -p tarnos-kcore -p tarnos-abi` green (45 tests, including
the renamed `pack_short_name`/`unpack_short_name` proptests). `cargo
clippy` clean across the kernel (both with and without `fat-fs-test`),
`tarnos-rt`, and every userland crate.

### Phase 4 — `SYS_SPAWN` filesystem fallback

Extends the existing spawn-by-name lookup to fall back to the
filesystem when the boot-module registry doesn't have a match.

**Exit condition:** a real boot spawns a program that exists only on
the FAT image, not in `limine.conf`'s own boot-module list, and it runs
to completion normally.

#### Findings

**Implementation.** `sys_spawn` tries `lookup_spawnable_module` first
(unchanged, cheap), then falls back to `fs::fat::with_root`'s own
`read_file` only on a miss -- an owned `Vec<u8>` referenced through a
plain `let owned; let elf_bytes: &[u8] = if ... { bytes } else {
owned = ...; &owned };`, since `Process::from_elf` only ever borrows its
input for the duration of one call, needing no `'static` lifetime the
boot-module case happens to have. Every distinct `fs::fat::FatError`
(no volume mounted, no such file, a corrupt chain, ...) collapses to
the one `SyscallError::NoSuchProgram` a caller already handles for the
boot-module-miss case -- none of that distinction is actionable from
`SYS_SPAWN`'s own caller. Needed no new kernel feature or capability at
all: `userland/init`'s own `probe_filesystem_spawn` runs unconditionally
in the real boot sequence (same tolerant-of-absence shape as Phase 3's
`probe_filesystem`), and `xtask test-fat-spawn` boots with **zero**
kernel test features -- the ordinary, unconditional real boot path,
proving the fallback on a completely normal boot.

**Test fixture reuse, and a real disk-image-size lesson.** Rather than
write a new userland crate just to prove the fallback mechanism, the
test copies the already-proven `exit-code-child` binary (Milestone 4,
`sys_exit(42)` and nothing else) onto the FAT image under the name
`FSCHILD.ELF` -- a name that appears nowhere in `limine.conf`, so any
successful spawn can only have come through the new fallback. Its
*debug* build (~2.9 MiB, almost entirely DWARF debug sections with no
`SHF_ALLOC` flag -- confirmed via `readelf -S`, none of it read by
`Process::from_elf`'s own PT_LOAD-segment loader) didn't fit a 1.44 MiB
floppy at all (`mcopy` failed with "Disk full"); `strip` (removing only
that non-loaded debug information) brought it down to 784 bytes.
`xtask`'s own `test_fat_spawn` now copies and strips it into
`build/fschild-stripped.elf` before building the disk image, rather
than switching to a release build (a second full `core`/`alloc`
compilation, more moving parts than one `strip` invocation).

**A second real, previously-latent bug, found by the exact same kind of
"first real exercise of an existing safe wrapper" pattern Phase 3's SSE
finding was**: the very first attempt reported `wait_killed` --
`ExitStatus::Killed`, not the `Exited(42)` the spawned child obviously
returned. Kernel-side tracing (temporary `earlyprintln!`s, removed once
diagnosed) showed `wait_for_child` correctly resolving `Exited(42)` and
writing it into the trap frame -- the kernel side was never wrong. The
bug was in `tarnos_rt::syscall::sys_wait` itself: it read `kind` from
`rsi` and `code` from `rdx`, but the kernel (`wait_for_child`'s
`AlreadyDone` branch and `apply_wake_result`'s `WaitCompleted` arm,
both checked directly) always writes `kind` into `rdi` and `code` into
`rsi`, never touching `rdx` at all. Misreading `code` (`42`, sitting in
`rsi`) as `kind` sent `ExitStatus::from_regs` down its `_ => Killed`
fallback arm every time. This exact wrapper has existed since Milestone
4 and was never once exercised: every existing `SYS_WAIT` test
(`test-wait-exit-code`, and every other scenario that waits on a child)
uses a raw-`asm!` dummy process reading the registers directly and
correctly, bypassing this function entirely -- `userland/init`'s own
`probe_filesystem_spawn` is the first *real, compiled* userland code in
the whole project ever to call `sys_wait` through its own safe wrapper.
Fixed by reading `kind` back out of `rdi` (`inout("rdi") target_pid =>
kind`) and `code` out of `rsi`, matching the kernel's actual, verified
convention exactly; confirmed by isolating the bug first with a
boot-module target (`exit-code-child` directly, no filesystem fallback
involved at all) to rule out anything Phase 4-specific before touching
`tarnos-rt`.

Full regression (clean rebuild): `test-smp-sched-concurrency` hit the
same pre-existing, environment-specific timing failure Phases 2/3
already root-caused; every other scenario in `test-all` (26
pre-existing plus `test-fat-parsing`/`test-fat-spawn`) individually
green, including `test-wait-exit-code` itself (confirming the
`sys_wait` fix changes nothing for the raw-`asm!` path that was already
correct). `cargo test -p tarnos-kcore -p tarnos-abi` green (45 tests).
`cargo clippy` clean across the kernel, `tarnos-rt`, every userland
crate, and `xtask`.

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

#### Findings

**New fixture, mirroring `block-child` exactly.** `userland/fs-child`
(new crate) reads `HELLO.TXT` through `tarnos_rt::syscall::sys_file_read`
and reports `FS_FIXTURE_OK`/`FAIL`, spawned directly via
`Process::from_elf` with `FS_CAP`/`CONSOLE_CAP` seeded straight into its
own table -- the same "real, independently-linked ELF binary, not a
kernel-copied function" bar `block-child` already set in Milestone 11.
Its own boot block (`fat-fixture-test`) has to call `fs::fat::mount_root`
itself, unlike the real boot sequence: this block ends in
`scheduler::start()` and never reaches the tail code where `main.rs`
normally calls it.

**Boundary probes.** `milestone12_tests::fat_boundary_process` (a dummy
process, same raw-`asm!`/`MaybeUninit`/top-level-`const` constraints as
`milestone11_tests::block_boundary_process`) runs four `SYS_FILE_READ`
checks against a real FAT12 disk holding only `HELLO.TXT`: a name that
doesn't exist (`NoSuchFile`, `-12`), a capability index nothing was
seeded into (`BadCapability`, `-2`), an unmapped destination buffer
(`InvalidArgument`, `-8`), and a destination buffer larger than the
file's own 46-byte size -- which must *succeed*, returning exactly
`46`, proving the length-bounded-prefix-read semantics `SYS_FILE_READ`'s
own doc comment describes are real, not just documented. All four
passed on the first real boot attempt; every register/name-packing
constant was independently verified with a short Python snippet before
being hand-transcribed into the `asm!` blocks, given this project's own
now-twice-recorded history of exactly this kind of register-mapping
bug (Phase 4's `sys_wait` finding) slipping past casual inspection.

**Log hygiene, not a bug:** both new scenarios' own disk images
include `BIGFILE.TXT` even though neither probe reads it, purely so
the real `init` process spawned alongside each dummy/fixture process
(every real boot spawns it unconditionally) finds its own Phase 3
`FS_CAP` probe fully satisfied too -- without it, `test-fat-boundary`'s
own first attempt showed a harmless but confusing `FS_SYSCALL_FAIL`
line from `init`'s unrelated check.

Full regression (clean rebuild): `test-smp-sched-concurrency` hit the
same pre-existing, environment-specific timing failure Phases 2-4
already root-caused; every other scenario in `test-all` (26
pre-existing plus this milestone's four) individually green. `cargo
clippy` clean across the kernel (default and both new features),
`tarnos-rt`, every userland crate including the new `fs-child`, and
`xtask`.

### Phase 6 — Documentation and sign-off

Write the closing ADR, confirm `test-kitchen-sink` is still correctly
excluded and still runnable standalone, and do a full clean-build
sign-off matching every prior milestone's own closing pattern.

**Exit condition:** ADR written; full regression green from a clean
build; both boot paths (BIOS/UEFI) verified.

#### Findings

`test-kitchen-sink` confirmed still correctly excluded from
`test-all`/CI and still runnable standalone: it reproduced the exact
same pre-existing, already-documented cross-core corruption panic
signature (`docs/adr/0012`-`0029`), the correct, expected, contained
outcome for this adversarial scenario — not a regression. Full clean
rebuild (`rm -rf build target`): zero build warnings, `cargo clippy`
clean across the entire workspace (kernel default and every feature
combination, `tarnos-rt`, every userland crate, both host-buildable
crates), `test-all` 30/30 (the pre-existing 26 plus this milestone's
four — `test-smp-sched-concurrency`'s own pre-existing, environment-
specific timing sensitivity, already confirmed via `git stash`
comparison in Phase 2 to be unrelated to any change this milestone
made, surfaced again during this final run and was individually
re-verified passing in isolation), both boot paths (BIOS via `test-all`
itself, UEFI via `test-uefi-boot`) verified in the same run, `cargo
test -p tarnos-kcore -p tarnos-abi` green (45 tests). `docs/adr/0032`
records the full closing summary; `docs/KNOWN-ISSUES.md`'s own stale
scenario-count references updated from 26 to 30.

Milestone 12 is done.

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
