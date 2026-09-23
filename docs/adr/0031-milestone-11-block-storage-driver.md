# 0031: Milestone 11 — PCI Enumeration and a Read-Only virtio-blk Driver

## Status

Accepted. Implemented across `kernel/src/arch/x86_64/{pci,syscall}.rs`,
`kernel/src/driver/{block,virtio_blk,mod}.rs`,
`kernel/src/memory/virt.rs`, `kernel/src/ipc/capability.rs`,
`kernel/src/task/scheduler.rs`, `kernel/src/milestone11_tests.rs`,
`kernel/src/main.rs`, `libs/tarnos-abi/src/lib.rs`,
`libs/tarnos-kcore/src/captable.rs`, `libs/tarnos-rt/src/syscall.rs`,
a new `userland/block-child` crate, `xtask/src/main.rs`,
`.github/workflows/ci.yml`, `limine.conf`, and the root `Cargo.toml`.
`docs/MILESTONE-11-BLOCK-STORAGE-DRIVER.md` records the full plan and
every phase's own detailed findings; this ADR is its closing summary,
matching every prior milestone's own pattern of ending with a written
record (most recently `docs/adr/0030`).

## Context

Milestone 10 certified the process/scheduler/IPC/memory core as a
stable base and used its own Phase 7 to check what a filesystem and
real drivers would actually need, finding three concrete gaps:
`driver::Driver`/`CharDevice` are genuinely UART-shaped with nothing
reusable for a block device; `KernelObjectRef` has exactly one variant
and `Rights` exactly two bits; and this kernel had no PCI bus support
at all — confirmed directly (`grep -rli pci kernel/src/` matched
nothing real) rather than assumed. A virtual disk on QEMU's default
`q35` machine model is a PCI device, so getting the kernel talking to
one needed PCI enumeration built first, as a real, sized prerequisite.

Filesystem and driver work were originally named together as "what
comes after Milestone 10." This milestone split them the same way
Milestone 6 split SMP bring-up from Milestone 7's actual cross-core
scheduling: get the kernel reading raw sectors off a virtual disk,
proven end to end through a real syscall and a real userland process,
under the same adversarial-testing bar every prior milestone has held
itself to — no filesystem, no on-disk format, no writes. That remains
Milestone 12's own job, on top of a driver already proven correct.

Two open questions from Milestone 10's Phase 7 are resolved here rather
than deferred again (see the milestone plan's own "Decisions" section
for the full reasoning): **drivers stay kernel-resident** (moving one
out to its own isolated process needs a currently-unbuilt MMIO/IRQ-via-
IPC primitive, not worth building for a second driver that doesn't
exist yet), and **a future filesystem stays kernel-resident too**, when
built (the alternative needs a `SYS_GRANT` extension — granting to an
already-running, unrelated process — with no proven need yet).

## Decision

### Phase 1 — Research and confirm the actual environment

Confirmed against this environment's real QEMU (8.2.2), not assumed
from the virtio/PCI specs: `virtio-blk-pci-non-transitional` (not plain
`virtio-blk-pci`, which is transitional and exposes a legacy I/O-port
interface too) is the right device — verified via `info pci` to expose
only two MMIO BARs, vendor `0x1AF4` device `0x1042` (the modern-only ID
range). The virtio capability → BAR/offset mapping and minimum feature
bits were deliberately *not* guessed from the spec and deferred to
Phase 2/3's own runtime capability-list read. `xtask` needs exactly two
new flags (`-drive ...,format=raw` plus
`-device virtio-blk-pci-non-transitional,drive=blk0`), confirmed by
booting the existing kernel image with them attached and observing a
completely unperturbed boot.

### Phase 2 — PCI enumeration

Implemented `arch::x86_64::pci`: Configuration Mechanism #1 access
(`CONFIG_ADDRESS`/`CONFIG_DATA`, ports `0xCF8`/`0xCFC`) via the same
`x86_64::instructions::port::Port` abstraction `interrupts.rs` already
uses for the PIT/PIC, and a brute-force `find_device` scan — deliberately
not a general PCI subsystem, matching the milestone's own scope. On a
real boot with the disk attached, the guest-side scan independently
rediscovered the device at bus 0/device 2/function 0 with the exact BAR
addresses QEMU's own host-side `info pci` had already reported.

### Phase 3 — virtio-blk driver

Implemented `driver::block::BlockDevice` (`capacity_sectors`,
`read_sectors`) and `driver::virtio_blk`: walks the device's real PCI
capability list (two small, purely PCI-generic additions to
`arch::x86_64::pci` — `capabilities`/`read_u8`/`read_u32` — with all
virtio-specific interpretation kept in `virtio_blk` itself) to locate
COMMON_CFG/NOTIFY_CFG/DEVICE_CFG, maps only the BAR bytes each
capability actually references at a fixed virtual address distinct from
the LAPIC's own (never HHDM for MMIO, matching `docs/adr/0009`),
negotiates only `VIRTIO_F_VERSION_1`, sets up one small virtqueue, and
performs a synchronous, polled, single-request sector read via three
dedicated DMA bounce-buffer frames. Passed its own kernel-internal
smoke test (read back a sector `xtask` seeded with known content) on
the first real boot attempt.

### Phase 4 — Capability and syscall surface

Added `KernelObjectRef::BlockDevice` (a pure marker — one singleton
device, reached through `virtio_blk::with_device`), `Rights::READ`,
`SyscallError::IoOutOfRange`/`IoError`, and `tarnos_abi::BLOCK_CAP`
(`CapIndex(2)`). Implemented `SYS_BLOCK_READ`
(`arch::x86_64::syscall::sys_block_read`) — this kernel's first syscall
that writes through a caller-supplied pointer rather than only
register-passed words or a kernel-chosen address. Every page the
destination buffer touches is validated present/writable/user-
accessible in the *caller's own* address space, via a new
`memory::virt::translate_in` that walks an arbitrary `pml4_frame`'s own
page tables (the existing global kernel-only mapper only ever reflects
the boot-time page tables, which don't cover any process's own user
half at all), before the device is ever touched; the actual copy goes
through each page's own physical/HHDM alias rather than a raw write
through the caller's pointer, deliberately not leaning on "`SYSCALL`
never switches `CR3`" as an unstated assumption. Made PCI enumeration
and driver bring-up unconditional (previously test-feature-gated),
matching `driver::uart::init()`'s own always-on precedent; confirmed
the added, guaranteed-worst-case-8192-iteration PCI scan doesn't
measurably affect boot timing against this project's existing (as
tight as 5s) scenario timeouts.

### Phase 5 — Userland fixture and adversarial testing

Added `userland/block-child`: a real, independently-linked ELF fixture
(unlike this milestone's own dummy processes, which are kernel
functions copied into a spawned process's own two pages and constrained
against ordinary Rust codegen) that reads a known sector via a new
`tarnos_rt::syscall::sys_block_read` wrapper and reports pass/fail.
Spawned directly at boot the same way `init` itself is
(`Process::from_elf`, not `SYS_SPAWN`) — deliberately not touching real
`init`'s own boot-wiring code, which is duplicated across roughly
fifteen existing, mutually-exclusive per-scenario boot paths that would
have needed retrofitting for a capability none of them test. Added
`block-boundary-test`: one dummy process running four adversarial
`SYS_BLOCK_READ` probes (out-of-range LBA, a capability index nothing
was seeded into, a zero sector count, an unmapped buffer address), each
checked against its *exact* expected error code. Two new `xtask`
scenarios, both passing on their first real boot attempt.

## Findings worth recording independent of the plan

- **A real, reusable class of bug in this project's own dummy-process
  testing technique, found twice in the same milestone.** A
  zero-initialized `[u8; 512]` stack array and a byte-string literal
  evaluated inside a runtime `if`/`else` both risk lowering, in this
  project's default *unoptimized* debug build, to a call or a
  static-data reference outside the two pages `Process::new_dummy`
  copies a dummy process's own compiled code into — jumping into
  whatever kernel code or data happens to sit next to it in memory.
  Neither had been exercised by any prior dummy process across ten
  milestones, because none needed a stack-local buffer of real size
  before this one. Found and fixed via a genuine page fault on Phase
  4's first real boot attempt (`block_read_syscall_process`); recognized
  and avoided *before* ever building or booting Phase 5's own
  `block_boundary_process`, which used the same fix (`MaybeUninit`, and
  top-level `const`s selected by a runtime `if` between two
  already-evaluated immediates) from the start and passed first try.
  Worth a permanent doc-comment note in `milestone11_tests.rs` itself
  (already added) for whoever writes the next one.
- **Transient host-performance variance, already documented in
  `docs/adr/0011`, surfaced repeatedly this milestone** —
  `test_smp_sched_concurrency` twice and `test_smp_kill_cross_core`
  once, across separate full-`test-all` runs during Phase 2/3
  verification, never the same scenario twice, never in code this
  milestone actually touches. Each was investigated per this project's
  own standing discipline (an isolated rerun, and for one case a
  `git stash` comparison against the prior commit) before being
  correctly attributed to the same known phenomenon rather than treated
  as a regression — this session's sandbox exhibited it broadly enough
  to occasionally hit a sequential `test-all` run, not just concurrent
  background jobs as `docs/adr/0011` originally characterized it.

## Consequences

- The kernel can now enumerate PCI devices, drive a real virtio-blk
  device end to end (feature negotiation, one virtqueue, synchronous
  polled reads), and expose that to userland through a capability-gated
  syscall with real, validated user-pointer handling — the first such
  mechanism in this codebase (`docs/adr/0003` is no longer accurate
  that "no user-pointer validation exists anywhere in this kernel").
- Milestone 12 (a minimal, read-only filesystem — FAT is the leading
  candidate, per the milestone plan's own "What happens after") can now
  build directly on a proven, tested block driver rather than alongside
  an unproven one.
- Both open Milestone 10 Phase 7 design questions (kernel-resident
  drivers; a future kernel-resident filesystem) are now explicit,
  recorded decisions rather than open items.
- Wiring `BLOCK_CAP` into the real `init` process (for a production
  `SYS_GRANT`-mediated flow to a userland-spawned client, rather than
  boot code seeding a dedicated fixture directly) remains deliberately
  undone — `tarnos_abi::BLOCK_CAP`'s own doc comment names this as open
  for whenever a concrete need (most likely Milestone 12's own
  filesystem server) actually arises.
- Read-only, single-request-at-a-time, polling-only, one fixed
  `MAX_SECTORS_PER_REQUEST` (8 sectors) per syscall — every one of this
  milestone's own stated Non-goals holds exactly as scoped; none were
  quietly expanded or discovered to be insufficient during
  implementation.
- Full regression (`test-all`, 26/26 — the pre-existing 22 plus this
  milestone's four) is green from a clean, from-scratch rebuild; the
  cross-core scheduling corruption bug (`docs/adr/0012`-`0029`) remains
  open, unaffected, and still correctly excluded from `test-all`/CI via
  `test-kitchen-sink`'s own standalone command, confirmed still
  runnable. Both boot paths (BIOS and UEFI) verified in the same clean
  run. `cargo clippy` clean across the entire workspace — every kernel
  feature configuration, `tarnos-rt`, every userland crate including
  the new `block-child`, and both host-buildable crates.
