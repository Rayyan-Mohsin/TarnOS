# TarnOS

TarnOS is an original microkernel operating system for x86_64, written in
Rust and booted via [Limine](https://github.com/limine-bootloader/limine).
It follows the seL4/L4 tradition: a small, privileged kernel that
provides address-space isolation, capability-based inter-process
communication, and a preemptible scheduler, with everything else —
drivers, filesystems, servers — built as ordinary user-mode processes on
top of a narrow syscall interface.

The project is built and verified milestone by milestone. Every
non-trivial design decision is recorded as a numbered Architecture
Decision Record under [`docs/adr/`](docs/adr/), and every syscall or
subsystem that claims to enforce a boundary — memory isolation,
capability rights, fault containment — has an adversarial integration
test written specifically to try to break it.

## Table of contents

- [Design principles](#design-principles)
- [Repository layout](#repository-layout)
- [Prerequisites](#prerequisites)
- [Building](#building)
- [Running](#running)
- [Testing](#testing)
- [System architecture](#system-architecture)
- [Syscall ABI reference](#syscall-abi-reference)
- [Capability model](#capability-model)
- [Project status](#project-status)
- [Known issues](#known-issues)
- [Documentation](#documentation)

## Design principles

- **Microkernel boundary.** The kernel provides address spaces,
  processes, capabilities, IPC, and scheduling. It does not provide a
  filesystem server, a network stack, or device policy — those are
  either built directly on kernel-exposed drivers (as the block and
  filesystem support currently is) or, in the seL4 tradition, intended
  to eventually run as ordinary user-mode servers.
- **Capabilities, not ambient authority.** No process can name or reach
  a kernel object — an IPC endpoint, a block device, a filesystem root
  — unless the kernel has explicitly placed a reference to it in one of
  that process's own capability table slots, with an explicit set of
  rights. There is no global, guessable, or forgeable object namespace.
- **Fail safe, not silent.** A user-mode fault kills only the offending
  process; a kernel-mode invariant violation halts cleanly with a
  diagnostic rather than continuing on possibly-corrupt state. See
  [Known issues](#known-issues) for the one place this discipline is
  still doing its job on an unresolved bug.
- **Confirm, don't assume.** Hardware and firmware behavior (QEMU's
  virtio-blk PCI layout, a FAT12 boot sector's real field offsets, a
  target's ABI requirements) is verified against the real environment
  before code is written against it, not taken on faith from a
  specification. The ADR trail documents this verification at every
  milestone where it mattered.

## Repository layout

```
.
├── kernel/                 tarnos-kernel — the kernel itself
│   └── src/
│       ├── arch/x86_64/    GDT/TSS/IDT, SYSCALL entry, LAPIC/SMP, PCI
│       ├── driver/         16550 UART, virtio-blk block driver
│       ├── fs/             FAT12 read-only filesystem
│       ├── ipc/            capability table, endpoints, message format
│       ├── memory/         physical/virtual memory management, heap
│       └── task/           process, scheduler, async executor
├── libs/
│   ├── tarnos-abi/         syscall ABI shared, unmodified, by kernel and userland
│   ├── tarnos-kcore/       host-testable core data structures (ring buffer,
│   │                       bitmap, capability table, IPC slot machine)
│   └── tarnos-rt/          userland runtime: entry point, panic handler,
│                           syscall wrappers, heap allocator
├── userland/
│   ├── init/               the first process, spawned directly by the kernel
│   ├── echo-child/         IPC round-trip fixture
│   ├── exit-code-child/    process lifecycle / wait fixture
│   ├── heap-child/         sys_sbrk / userland heap fixture
│   ├── block-child/        virtio-blk / SYS_BLOCK_READ fixture
│   └── fs-child/           FAT12 / SYS_FILE_READ fixture
├── xtask/                  build, ISO assembly, and QEMU-based integration
│                           tests (`cargo run -p xtask -- <command>`)
├── targets/                custom no_std target specification for userland
├── docs/
│   ├── adr/                numbered architecture decision records
│   ├── KNOWN-ISSUES.md     the one open bug worth knowing about
│   └── MILESTONE-*.md      per-milestone plans and phase-by-phase findings
├── limine.conf             bootloader configuration and boot-module list
└── rust-toolchain.toml     pinned nightly toolchain
```

## Prerequisites

TarnOS builds against a pinned nightly Rust toolchain and two `no_std`
targets — the kernel's built-in bare-metal target and a custom JSON
target for userland — so it cannot be built with a stable toolchain or
a plain `cargo build`. Install:

- **Rust**, via [rustup](https://rustup.rs/). `rust-toolchain.toml`
  pins the exact nightly and components (`rust-src`, `llvm-tools`,
  `clippy`); running any `cargo`/`rustup` command inside the repository
  installs it automatically.
- **QEMU** (`qemu-system-x86`) — every integration test and the `run`
  command boot a real virtual machine.
- **xorriso** — assembles the bootable ISO image.
- **OVMF** — UEFI firmware, required for `--uefi` boots.
- **mtools** (`mformat`, `mcopy`) — builds the FAT12 disk images used by
  the block-driver and filesystem test scenarios.

See [`.github/workflows/ci.yml`](.github/workflows/ci.yml) for the exact
package names on Ubuntu; the same workflow runs on every push.

## Building

`xtask` is the only supported entry point for compiling the kernel or
any userland binary — they build against `no_std` targets (the kernel's
built-in bare-metal target, userland's own custom JSON target) that a
bare `cargo build` at the workspace root does not select.

```sh
cargo run -p xtask -- build          # cross-compile the kernel + every userland binary
cargo run -p xtask -- iso            # also assemble build/tarnos.iso
```

## Running

```sh
cargo run -p xtask -- run            # build (if needed) and boot in QEMU via legacy BIOS
cargo run -p xtask -- run --uefi     # boot the same image via OVMF/UEFI instead
```

A normal boot brings up every reported CPU core, mounts a FAT12 volume
if one is attached, spawns `init`, and hands off; `init` exercises the
IPC, process-lifecycle, block, and filesystem syscalls against whatever
capabilities it was actually seeded with, tolerant of anything absent
(such as no disk being attached at all).

## Testing

```sh
cargo run -p xtask -- test-all                      # every integration scenario (30), each boots a real QEMU instance
cargo test -p tarnos-kcore -p tarnos-abi            # host-side unit + property tests, no QEMU needed
cargo clippy -p tarnos-kcore -p tarnos-abi --all-targets -- -D warnings
```

Run `cargo run -p xtask --` with no arguments for the full, individually
documented list of `test-*` scenarios. Each one boots a purpose-built
kernel variant (selected via a Cargo feature) and asserts on its
captured serial output — for example:

| Scenario | What it proves |
|---|---|
| `test-fault-isolation` | A user-mode fault kills only the offending process, not the kernel |
| `test-spawn-boundary` | `SYS_GRANT`/`SYS_PROCESS_START` reject a target that isn't the caller's suspended child |
| `test-smp-kill-cross-core` | `SYS_KILL` can evict a process running on a *different* core |
| `test-block-boundary` | Out-of-range, missing-capability, and malformed `SYS_BLOCK_READ` calls are rejected |
| `test-fat-boundary` | A missing file, an unseeded capability, an unmapped buffer, and a past-end read are each handled correctly by `SYS_FILE_READ` |

One additional scenario, `test-kitchen-sink`, is **deliberately excluded**
from `test-all` and CI — it is the reproduction for the open bug
described in [Known issues](#known-issues), not a real workload, and its
failure rate would make every push look broken regardless of whether it
changed anything.

## System architecture

**Boot.** Limine loads the kernel and any configured boot modules,
hands off in long mode, and the kernel brings up its own GDT/TSS/IDT,
physical and virtual memory management, and a kernel heap before
bringing up any additional CPU cores reported by the firmware. Each
core is then handed to a preemptible, per-core scheduling loop.

**Processes and scheduling.** A process is an address space plus a
fixed-size capability table. The scheduler is a preemptible round-robin
design with fixed-size, allocation-free ready-queue and process-table
structures on the hot preemption path — the timer-tick handler runs
with interrupts disabled and must never allocate. Processes can spawn
children (`SYS_SPAWN`), transfer capabilities to a suspended child
before releasing it (`SYS_GRANT`/`SYS_PROCESS_START`), block waiting for
a child to exit (`SYS_WAIT`), and be killed by their parent at any point
in their lifecycle (`SYS_KILL`). All of this works correctly across
cores: a process can wait on, send to, or kill a process currently
running on a different CPU.

**IPC.** Communication is synchronous, rendezvous-style message passing
over capability-addressed endpoints — a sender blocks until a receiver
is ready and vice versa, mirroring seL4's own IPC model. A small
message (a tag plus four inline words) is passed entirely in registers.

**Memory.** Every mapping goes through one safe, typed module
(`memory::virt`); nothing above it walks or dereferences a raw page
table entry directly. Kernel stacks are guard-paged. A process's heap
grows on demand via `SYS_SBRK`, backed by `tarnos-rt`'s own userland
allocator.

**Drivers.** A 16550 UART provides the kernel's own diagnostic console.
A from-scratch virtio-blk driver — targeting the virtio 1.0+ "modern"
PCI interface, discovered through a minimal PCI configuration-space
scanner — provides synchronous, read-only sector access to a real block
device, exposed to user-mode through a capability-gated syscall.

**Filesystem.** A read-only FAT12 parser is built directly on top of
the block driver's own minimum guaranteed contract (single-sector
reads), doing its own cluster-chain traversal and 8.3 directory lookup
rather than reaching into driver internals. `SYS_SPAWN` falls back to
loading a program's ELF image directly off this filesystem when no
matching boot module exists, so a new program can be added to a disk
image without rebuilding the kernel's boot configuration.

## Syscall ABI reference

Every syscall number, argument order, and error code below is defined
once, in `libs/tarnos-abi`, and shared unmodified by the kernel and
every userland binary — the two sides of the trap boundary cannot drift
out of sync with each other. On return, `rax` holds a non-negative
result on success or `-(error code)` on failure, the same convention
Linux's x86_64 syscall ABI uses.

| # | Syscall | Signature | Description |
|---|---|---|---|
| 0 | `SYS_YIELD` | `()` | Voluntarily give up the remaining timeslice |
| 1 | `SYS_SEND` | `(cap, tag, w0, w1, w2, w3)` | Rendezvous-send a message on a capability |
| 2 | `SYS_RECV` | `(cap)` | Rendezvous-receive a message on a capability |
| 3 | `SYS_EXIT` | `(code)` | Terminate the calling process |
| 4 | `SYS_SPAWN` | `(name_lo, name_hi, name_len)` | Create a suspended child process from a named program |
| 5 | `SYS_GRANT` | `(target_pid, src_cap, dest_cap, rights)` | Clone a capability into a suspended child's table, narrowed to `rights` |
| 6 | `SYS_PROCESS_START` | `(target_pid)` | Release a suspended child into the ready queue |
| 7 | `SYS_WAIT` | `(target_pid)` | Block until a child exits; returns its exit status |
| 8 | `SYS_KILL` | `(target_pid)` | Immediately terminate a child, in any state |
| 9 | `SYS_SBRK` | `(increment)` | Grow the caller's heap; returns the previous break |
| 10 | `SYS_BLOCK_READ` | `(cap, lba, buf_ptr, sector_count)` | Read whole 512-byte sectors from a block device |
| 11 | `SYS_FILE_READ` | `(cap, name_lo, name_hi, name_len, buf_ptr, buf_len)` | Read a named file's content, truncated to `buf_len` |

Program and file names are passed by value, packed into two 64-bit
registers plus a length (`pack_short_name`/`unpack_short_name`), never
by a raw user pointer — there is currently no general user-pointer
string validation in the kernel, so short, fixed-width names are passed
the same way a small IPC message is. Destination buffers for
`SYS_BLOCK_READ` and `SYS_FILE_READ` *are* raw user pointers; every byte
of the target range is validated as present, writable, user-accessible
memory before the underlying device is touched.

## Capability model

A capability is an index into the *calling process's own* table; there
is no global object namespace to guess or forge. A boot-seeded process
starts with a fixed set of well-known slots:

| Index | Constant | Grants |
|---|---|---|
| 0 | `CONSOLE_CAP` | `SEND` on the console server's endpoint |
| 1 | `CHILD_LINK_CAP` | `SEND \| RECV` on a fresh endpoint, for talking to a spawned child |
| 2 | `BLOCK_CAP` | `READ` on the attached virtio-blk device, when present |
| 3 | `FS_CAP` | `READ` on the mounted FAT12 volume's root, when one was found at boot |

A spawned child starts with an *empty* capability table and receives
only what its parent explicitly grants it via `SYS_GRANT` — authority is
never ambient or inherited by default.

## Project status

Twelve milestones have been completed, each closed with its own ADR:

1. Boot via Limine; GDT/TSS/IDT with double-fault handling;
   physical/virtual memory management and a kernel heap; capability-based
   IPC, an ELF64 loader, and the process/scheduler core (`docs/adr/0001`-`0004`)
2. Fault isolation and genuine blocking IPC (`docs/adr/0005`)
3. Dynamic process creation and capability transfer:
   `SYS_SPAWN`/`SYS_GRANT`/`SYS_PROCESS_START` (`docs/adr/0006`)
4. Process lifecycle and termination: `SYS_WAIT`/`SYS_KILL` (`docs/adr/0007`)
5. A userland heap via `SYS_SBRK` (`docs/adr/0008`)
6. Symmetric multiprocessing bring-up: every reported core boots and
   runs concurrently (`docs/adr/0009`)
7. Cross-core scheduling: processes migrate, block, and are killed
   correctly across cores (`docs/adr/0010`)
8. Per-core LAPIC timer hardening and forced preemption (`docs/adr/0011`)
9. An investigation opened into a cross-core scheduling corruption bug
   found under adversarial stress testing; the root cause has not been
   found and the bug remains open, isolated to one deliberately
   excluded test scenario (`docs/adr/0012`-`0029`; see
   [Known issues](#known-issues))
10. A full audit pass — correctness, lints, unsafe-code review, test and
    documentation coverage — across the entire codebase before building
    further on the core (`docs/adr/0030`)
11. A PCI-enumerated, synchronous virtio-blk driver and a
    capability-gated block-read syscall surface (`docs/adr/0031`)
12. A read-only FAT12 filesystem, a capability-gated file-read syscall,
    and a filesystem-backed fallback for `SYS_SPAWN` (`docs/adr/0032`)

Every milestone's plan, phase-by-phase findings, and closing decision
record live under [`docs/`](docs/) and [`docs/adr/`](docs/adr/)
respectively — see [Documentation](#documentation).

## Known issues

A cross-core scheduling corruption bug has been under active,
methodical investigation since Milestone 9 (`docs/adr/0012` through
`docs/adr/0029`). It requires multiple cores and sustained, heavy
scheduling pressure to reproduce, fails safely (a controlled panic and
halt, never silent corruption), and has never been observed in any of
`test-all`'s 30 ordinary scenarios or in normal use. It is deliberately
reproduced, on demand, by its own standalone `test-kitchen-sink`
scenario rather than gated into CI. Full detail, reproduction
instructions, and the current state of the investigation are in
[`docs/KNOWN-ISSUES.md`](docs/KNOWN-ISSUES.md).

## Documentation

- [`docs/adr/`](docs/adr/) — the numbered decision record for every
  non-trivial design choice in this codebase, in chronological order.
  Start at `0001` for the original microkernel/capability boundary, or
  at the highest number for the most recent milestone's own summary.
- [`docs/KNOWN-ISSUES.md`](docs/KNOWN-ISSUES.md) — the open cross-core
  bug: reproduction, scope, and investigation status.
- [`docs/MILESTONE-10-LAST-BASE-LEVEL-CHECK.md`](docs/MILESTONE-10-LAST-BASE-LEVEL-CHECK.md),
  [`docs/MILESTONE-11-BLOCK-STORAGE-DRIVER.md`](docs/MILESTONE-11-BLOCK-STORAGE-DRIVER.md),
  [`docs/MILESTONE-12-FILESYSTEM.md`](docs/MILESTONE-12-FILESYSTEM.md) —
  the plan and phase-by-phase findings for each of the three most recent
  milestones.
