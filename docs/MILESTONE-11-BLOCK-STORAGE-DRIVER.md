# Milestone 11: PCI Enumeration and a Read-Only virtio-blk Driver

## Why this milestone exists

Milestone 10 certified the process/scheduler/IPC/memory core as a
stable base and used its own Phase 7 to check what a filesystem and
real drivers would actually need. Three concrete findings from that
check shape this milestone directly:

- `driver::Driver`/`CharDevice` are genuinely UART-shaped (byte at a
  time, no addressing, no async completion) — nothing there is reusable
  for a block device. A `BlockDevice`-shaped trait needs designing from
  scratch.
- `KernelObjectRef` has exactly one variant (`Endpoint`) and `Rights`
  exactly two bits (`SEND`/`RECV`) — a device capability needs both
  extended, exactly the extension point `docs/adr/0001` designed
  `KernelObjectRef` to have but never used.
- Whether drivers stay kernel-resident or move out-of-process, and
  whether a filesystem server needs to grant capabilities to arbitrary
  already-running clients, were left as open decisions for whichever
  milestone actually builds this. This milestone makes both decisions
  explicitly rather than deferring them again — see Decisions below.

A fourth thing, not mentioned in Milestone 10's own findings because it
falls outside "does the existing interface generalize": **this kernel
has no PCI bus support at all today** (confirmed directly this round —
`grep -rli pci kernel/src/` matches nothing real; the RSDP is captured
at boot per `docs/adr/0001` but never parsed beyond a log line). A
virtual disk on QEMU's default `q35` machine model is a PCI device.
Getting the kernel talking to it needs PCI configuration-space access
and enumeration built first — a real prerequisite this milestone must
size and build, not assume away.

Filesystem and driver work were named together as "what comes after
Milestone 10," but combining them into one milestone repeats a mistake
this project already corrected once: Milestone 6 split SMP bring-up
(boot every core, minimal per-core infrastructure) from Milestone 7
(actually scheduling processes across those cores) specifically
because of how much full SMP touched at once. Storage is the same
shape — a working, tested block driver is a large, self-contained piece
on its own; a filesystem format, path resolution, and
filesystem-backed process loading are a second large piece that should
be built *on top of* a driver already proven correct, not alongside it
while the driver itself is still unproven. This milestone is the first
half only: **get the kernel reading raw sectors off a virtual disk,
proven end to end through a real syscall and a real userland process,
under the same adversarial-testing bar every prior milestone has held
itself to.** No filesystem, no on-disk format, no writes.

## Decisions

Two open questions from Milestone 10's Phase 7 are resolved here,
explicitly, rather than carried forward again:

**Drivers stay kernel-resident.** Moving a driver out to its own
isolated process needs a currently-unbuilt primitive — some way for a
process to safely receive MMIO access and hardware interrupt delivery
via IPC — and building that primitive well is itself a milestone-sized
effort with no second driver yet to justify the design cost. The UART
driver already established the precedent of starting kernel-resident
(`driver/mod.rs`'s own doc comment: "the seed of a future out-of-process
driver manager, not a one-off") and this milestone follows the same
path for exactly the same reason. Revisit once there are enough
drivers, or a strong enough isolation requirement, to justify the
MMIO/IRQ-via-IPC design work on its own.

**A future filesystem stays kernel-resident too, when it's built** (not
decided in detail this milestone, since no filesystem code is written
here — recorded now so the next milestone doesn't have to re-litigate
it). The alternative — a filesystem-as-a-userspace-server model — needs
a process to grant a capability to an *already-running, unrelated*
client, which the current `SYS_GRANT` (parent-to-`Suspended`-child
only) cannot express. Building that mechanism is a real, separate
piece of design work with no proven need yet; starting with an
in-kernel filesystem (reusing exactly the same capability-propagation
pattern this milestone's own block-device capability uses) defers that
question until it's actually blocking something.

## Scope

- PCI configuration-space access and enumeration (new
  `kernel/src/arch/x86_64/pci.rs` or similar) — enough to find one
  specific device by vendor/device ID and read its resources. Not a
  general-purpose PCI subsystem; scoped to what finding a virtio-blk
  device needs.
- A new `driver::block` module: a `BlockDevice` trait shaped for
  addressed, whole-sector I/O (not `CharDevice`'s byte-at-a-time
  shape), designed from scratch per Phase 7's own finding.
- A virtio-blk driver implementing that trait: feature negotiation, one
  virtqueue, synchronous (polled, not interrupt-driven) single-request
  reads. No writes.
- `KernelObjectRef` gains a device-capability variant; `Rights` gains
  whatever bit(s) a block-read capability needs; `tarnos_abi::SyscallError`
  gains I/O-error variant(s) — the exact extensions Phase 7 named as
  missing.
- One new syscall (name TBD during Phase 3 — e.g. `SYS_BLOCK_READ`)
  reading N sectors at a given LBA into a process-supplied buffer,
  gated by a capability boot code seeds the same way `CONSOLE_CAP` is
  seeded today.
- A new userland test fixture exercising it, plus `xtask`/CI changes:
  attaching a disk image to the QEMU invocation, seeding it with known
  test data at known sectors, and new adversarial `test-*` scenarios.

## Non-goals

- **No filesystem, no on-disk format of any kind.** Reading is by raw
  LBA sector number. Milestone 12's own job, once this driver is
  proven.
- **No writes to the block device.** Read-only — smaller surface,
  smaller blast radius if something's wrong, and nothing built so far
  needs write support yet.
- **No out-of-process driver.** See Decisions above.
- **No interrupt-driven I/O.** The driver polls for virtqueue
  completion synchronously, the same "simplest correct thing first"
  choice the original UART driver made before this project ever built
  anything async. Revisit once something actually needs non-blocking
  storage I/O.
- **No general PCI driver framework.** Enough enumeration to find one
  device by ID, not a registry other future drivers are assumed to use
  — that generalization can happen once a second PCI device actually
  needs it.

## Phases

### Phase 1 — Research and confirm the actual environment

Before writing driver code, confirm directly (this project's own
standing discipline — "confirmed directly, not assumed," per
`docs/adr/0009`'s own LAPIC-via-HHDM lesson) rather than assume from
general virtio/PCI knowledge:

- Whether QEMU's `-M q35` (already `xtask`'s own machine model) exposes
  PCI configuration space via the legacy 0xCF8/0xCFC port-I/O mechanism
  without needing any ACPI table lookup first — this is the expected
  case (legacy mechanism access doesn't need ACPI/MCFG), but confirm it
  boots and enumerates before committing to it over the MCFG/ECAM
  alternative.
- Exactly what `xtask`'s QEMU invocation needs to add — a `-drive
  file=...,if=none` plus `-device virtio-blk-pci` (or equivalent) — to
  attach a disk image at all, and how large/what format that image
  needs to be for the simplest working setup.
- The virtio-blk device's exact PCI capability layout (common config,
  notify, ISR, device-specific config regions) for the "modern"
  (virtio 1.0+) interface QEMU presents by default, and the minimum
  feature bits this driver actually needs to negotiate for a working
  single-queue synchronous read.

**Exit condition:** a short written note (this doc or the eventual ADR)
confirming each of the above against a real, booted QEMU instance —
not copied from a spec — before Phase 2 starts.

#### Findings

Confirmed against this environment's actual QEMU (8.2.2), not assumed:

- **The exact device to use: `virtio-blk-pci-non-transitional`**, not
  plain `virtio-blk-pci`. QEMU's `-device help` lists three PCI
  variants; the plain one defaults to "transitional" (presents a
  legacy I/O-port-BAR interface *and* the modern capability-based one,
  switched via a feature bit, so a driver would need to handle both
  shapes or explicitly negotiate modern-only). The
  `-non-transitional` variant presents *only* the modern interface —
  confirmed via `info pci` on a running instance (see below): no
  I/O-port BAR at all, only two MMIO BARs. This is the right choice for
  a driver that only ever wants to speak one, simpler protocol shape.
- **Enumeration identity, read back live via QEMU's monitor
  (`info pci`) with the device attached**: vendor `0x1AF4`, device
  `0x1042` — exactly the "modern-only" block-device ID the virtio 1.x
  spec defines (`0x1040 + device-type 2`), not the legacy transitional
  range (`0x1000`-`0x103F`). Two BARs: BAR1 (32-bit MMIO, 4 KiB) and
  BAR4 (64-bit prefetchable MMIO, 16 KiB) — confirming this driver only
  ever needs to map ordinary MMIO, the exact same "explicit fixed
  virtual address, never HHDM" pattern already established for the
  LAPIC (`docs/adr/0009`), not a fixed layout assumption: **which PCI
  capability (common/notify/ISR/device config) lives in which BAR at
  which offset is not fixed by the spec and must be read from the
  device's own PCI capability list at Phase 2/3 runtime**, not
  hardcoded from this one observation.
- **IRQ 11, pin A (legacy INTx) is available**, confirming this
  milestone's own Non-goal (no interrupt-driven I/O) is a free choice,
  not a forced one — the device works with legacy interrupts if a
  future milestone wants them; this one simply never unmasks or
  registers a handler for it, matching the synchronous-polling scope.
- **`xtask` needs exactly two new flags**, confirmed by booting the
  existing kernel image with them attached and observing a normal,
  unperturbed boot (identical log output to every other scenario,
  through init spawning `echo-child` and a clean halt):
  `-drive file=<path>,if=none,format=raw,id=blk0` plus `-device
  virtio-blk-pci-non-transitional,drive=blk0`. A plain, small (a few
  MiB) raw disk image is sufficient — `format=raw` needs no special
  tooling to create (`dd if=/dev/zero of=... bs=1M count=N`) or to seed
  with known test data at known offsets, which Phase 5 needs.
- **Recommend also adding `-nic none`** to scenarios that attach the
  disk (and, separately, worth considering for every other scenario
  too, since this kernel has no network stack at all): confirmed it
  removes an Ethernet controller from the PCI bus with no effect on
  anything else, simplifying what the new enumeration code sees during
  its own testing. Not required, just lower-noise.
- **Legacy PCI configuration access (ports `0xCF8`/`0xCFC`) needs no
  further environment confirmation beyond what's already established**:
  this is a mandatory, unconditional part of the PC-compatible platform
  standard (unlike the LAPIC/HHDM assumption `docs/adr/0009` had to
  correct, which was a Limine-specific promise, not a hardware
  guarantee) — supported by every PCI/PCIe chipset ever built,
  Q35/ICH9 included, with no ACPI dependency. The actually meaningful
  verification is Phase 2's own guest-side code reading it back
  successfully on a real boot, which is that phase's own exit
  condition, not something further host-side probing can usefully add.
- **Deferred to Phase 2/3, not resolved here**: the exact virtio
  capability→BAR→offset mapping and the minimum feature bits to
  negotiate. Guessing these from the spec alone risks exactly the kind
  of unverified assumption this project's own standing discipline
  warns against; the guest driver will read the real capability list
  directly once it can, per Phase 2/3's own exit conditions.

### Phase 2 — PCI enumeration

Raw config-space reads (port I/O to 0xCF8/0xCFC, per Phase 1's
confirmation), enumerating bus/device/function far enough to find the
virtio-blk device by its vendor ID (`0x1AF4`) and appropriate device
ID, and read back its BARs. Logged, empirically, the same "prove it
found what it claims to have found" discipline `arch::x86_64::smp`
already established for AP bring-up — not assumed correct because it
compiled.

**Exit condition:** a boot log line naming the discovered device's
bus/device/function and BAR addresses, on a real QEMU boot with the
disk device attached.

#### Findings

- **Implemented** `arch::x86_64::pci`: PCI Configuration Mechanism #1
  access (`CONFIG_ADDRESS`/`CONFIG_DATA` via `x86_64::instructions::port::Port<u32>`,
  matching the same crate abstraction `interrupts.rs` already uses for
  the PIT/PIC), a brute-force `find_device(vendor_id, device_id)` scan
  (bus `0..=255`, device `0..32`, function `0..8` when the header type's
  multi-function bit is set), and `PciDevice::mmio_bar_address`/
  `enable_mmio_and_bus_master` for the one real device this milestone
  needs. Gated behind a new `block-driver-test` feature's boot block in
  `main.rs`, the same pattern every prior milestone's diagnostic-only
  scenarios use.
- **Exit condition met on a real boot**: with xtask attaching
  `virtio-blk-pci-non-transitional` exactly as Phase 1 confirmed, the
  guest-side `find_device(0x1AF4, 0x1042)` call independently
  rediscovered the device at `PciAddress { bus: 0, device: 2, function: 0 }`
  and decoded `BAR1 = 0xfebf1000` (32-bit MMIO) and `BAR4 = 0xfe000000`
  (64-bit MMIO) — matching Phase 1's own host-side `info pci` findings
  exactly, with zero perturbation to the rest of the boot sequence.
  `[pci-test] PCI_ENUM_OK` printed and boot proceeded normally.
- **Zero warnings in both configurations**: default build and
  `--features block-driver-test` both build and clippy (`-D warnings`)
  clean. The module carries a temporary
  `#![cfg_attr(not(feature = "block-driver-test"), allow(dead_code))]`
  (unlike Milestone 10's permanent dead-code justifications) — Phase
  3/4 will make `find_device` an unconditional call from the real
  driver, at which point this attribute comes back out.
- **Two different `test-all` flakes investigated and cleared, neither
  in code Phase 2 touches**: across repeated full-suite verification
  runs this phase, `test_smp_sched_concurrency` failed once and, on a
  separate run, `test_smp_kill_cross_core` failed once — never the
  same scenario twice, and neither anywhere near PCI code. Per this
  project's own standing discipline (ADR 0011: verify a suspicious
  failure with an isolated rerun before treating it as a regression),
  both were investigated rather than dismissed or blamed on Phase 2:
  `test_smp_sched_concurrency`'s isolated rerun failed identically; a
  `git stash` back to the prior commit (zero Phase 2 code) reproduced
  the identical failure, proving Phase 2 wasn't the cause; an extended
  40-second-timeout run (vs. the normal 15s) still didn't complete,
  ruling out "just needs more time"; and this same scenario had passed
  cleanly during Milestone 10 Phase 9's sign-off only two turns
  earlier in this session. `test_smp_kill_cross_core` passed 3/3 when
  rerun in isolation immediately after its one full-suite failure.
  Conclusion: transient host-performance variance (the same phenomenon
  ADR 0011 already documented, previously only observed with
  concurrent background QEMU jobs) — this session's sandbox is
  exhibiting it broadly enough to occasionally hit a sequential
  `test-all` run too, on whichever scenario happens to be timing-
  sensitive at that moment, not a code regression tied to any
  scenario or to Phase 2's changes. A final clean run passed
  `test-all` 22/22 with exit code 0.

### Phase 3 — virtio-blk driver

Map the discovered device's capability list, negotiate the minimum
feature set Phase 1 identified, set up one virtqueue, and implement a
synchronous (poll-until-complete) single-sector (or small, fixed
multi-sector) read into a kernel-owned buffer. `BlockDevice`'s trait
shape (methods, error type) designed here, informed by what this one
real driver actually needs — not speculatively generalized for a
second, hypothetical block device that doesn't exist yet.

**Exit condition:** a kernel-internal (no syscall yet) smoke test reads
a sector with known content (seeded into the test disk image by
`xtask`) and the content matches, on a real boot.

#### Findings

- **Implemented `driver::block`**: a minimal `BlockDevice` trait
  (`capacity_sectors`, `read_sectors`) and a small `BlockError` enum
  (`BufferNotSectorAligned`/`OutOfRange`/`RequestTooLarge`/`DeviceError`)
  — designed after, and only after, writing the one real driver against
  it, per this phase's own instruction not to speculatively generalize.
- **Implemented `driver::virtio_blk`**: walks the device's real PCI
  capability list (via two small, purely-generic additions to
  `arch::x86_64::pci` — `PciDevice::capabilities`/`read_u8`/`read_u32` —
  deliberately kept virtio-agnostic, with all virtio-specific
  interpretation staying in `virtio_blk` itself) to locate
  COMMON_CFG/NOTIFY_CFG/DEVICE_CFG; maps only the BAR bytes each
  capability actually references (never a hardcoded whole-BAR size) at
  a fixed virtual address distinct from the LAPIC's own, the same
  "never HHDM for MMIO" reasoning `docs/adr/0009` established; negotiates
  the one required feature bit (`VIRTIO_F_VERSION_1`) and nothing more;
  sets up one virtqueue (queue 0, clamped to a small fixed size — a
  single 3-descriptor request chain never needs more); and performs a
  synchronous, polled (no interrupt handler ever registered, matching
  this milestone's own Non-goal), single-request-at-a-time sector read
  using three dedicated DMA bounce-buffer frames (header/data/status).
  Every capability/BAR/feature/queue value is read from the device
  itself at runtime — nothing from Phase 1's own observations is
  hardcoded, per that phase's own explicit deferral.
- **Exit condition met on the first real boot attempt**: extended the
  `block-driver-test` feature's boot block to call `virtio_blk::init()`
  and read back sector 2 (a fixed convention shared with `xtask`, not a
  derived constant — the two are separate crates with no shared build-
  time config for a single test value); the content matched the
  `0..=255`-repeating pattern `xtask`'s new `create_test_disk_image`
  seeds there, byte for byte, immediately, with zero debugging needed —
  `[blk-test] BLOCK_READ_OK`. Confirmed reliable across three repeated
  boots, not a one-off.
- **New `xtask test-block-driver` scenario** (wired into `test-all`/CI):
  builds a small raw disk image with `xtask`'s own new
  `create_test_disk_image` helper, attaches it with the exact
  `virtio-blk-pci-non-transitional` flags Phase 1 confirmed plus
  `-nic none`, and asserts both `PCI_ENUM_OK` and `BLOCK_READ_OK`. Needed
  a small, additive `run_scenario_ext` (extra raw QEMU args) alongside
  the existing `run_scenario`, rather than changing that function's
  signature and touching its ~20 existing call sites.
- **Zero warnings in both configurations**: default build and
  `--features block-driver-test` both build and clippy (`-D warnings`)
  clean, after trimming a few genuinely-unneeded additions along the way
  (an unused `PciDevice::read_u16`, two `VirtioBlk` fields that turned
  out to never be read after construction) rather than blanket-allowing
  them. `driver::block`/`driver::virtio_blk` both carry the same
  temporary `#[cfg_attr(not(feature = "block-driver-test"), allow(dead_code))]`
  pattern `arch::x86_64::pci` established in Phase 2, for the same
  reason: Phase 4 makes this driver's `init` unconditional, at which
  point the attribute comes back out.
- **`test_smp_sched_concurrency` flaked again during full-suite
  verification** — a third occurrence of the same pre-existing,
  host-performance-variance phenomenon Phase 2's own Findings already
  documented (and, before that, ADR 0011). Nothing in this phase touches
  scheduling, PCI-unrelated code paths were the only thing that changed.
  Confirmed via three isolated reruns (3/3 pass) before treating it as
  the same known non-regression; a subsequent clean run passed the full
  suite (23/23, including the new `test-block-driver`) with exit code 0.
- **xtask's own clippy status left as found**: running clippy directly
  against the `xtask` crate (never gated by CI, which only lints
  `tarnos-kcore`/`tarnos-abi` and the cross-compiled kernel/userland
  targets) surfaced one pre-existing lint (`manual_contains`, in
  unrelated Milestone 6/7 SMP-boot code, likely a newer clippy version
  than whenever that code was last touched) — confirmed, by re-running
  with that one lint suppressed, that none of this phase's own new xtask
  code triggers anything. Left alone as out of this phase's scope,
  rather than fixing unrelated code a routine check happened to surface.

### Phase 4 — Capability and syscall surface

`KernelObjectRef` gains its device-capability variant; `Rights` gains
its new bit(s); `SyscallError` gains I/O-error variant(s). The new
syscall (block-read) is gated by a capability seeded into a boot
process's table exactly the way `CONSOLE_CAP` is today, and validates
its inputs (LBA range, buffer bounds) with the same rigor `sys_sbrk`'s
own boundary checks already establish as this codebase's bar.

**Exit condition:** the same content-matches smoke test as Phase 3, now
reached through a real syscall from a real (dummy or ELF) process
instead of a kernel-internal call.

#### Findings

- **`KernelObjectRef` gains `BlockDevice`** — a pure marker variant (no
  embedded state: there is exactly one virtio-blk device, reached
  through `driver::virtio_blk::with_device`'s own singleton). `Rights`
  gains `READ` (bit `0b100`). `SyscallError` gains `IoOutOfRange` and
  `IoError`. `tarnos_abi::BLOCK_CAP` (`CapIndex(2)`) is the fixed,
  well-known index this milestone reserves, analogous to
  `CONSOLE_CAP`/`CHILD_LINK_CAP` — wiring it into the real `init`
  process (for Phase 5's userland fixture to receive via `SYS_GRANT`)
  is deliberately left to that phase, not done here: Phase 4's own exit
  condition explicitly allows a dummy process, and retrofitting all
  ~15 existing feature-gated boot paths that each spawn `init`
  separately for a capability nothing yet uses would have been risk
  with no test value this phase.
- **`SYS_BLOCK_READ` implemented** as `arch::x86_64::syscall::sys_block_read`:
  resolves the capability and validates the caller-supplied buffer in
  one short critical section (never holding `SCHEDULER` across the
  driver's own polling loop, mirroring `sys_sbrk`'s own documented
  reasoning), then performs the device read and copies into the
  caller's buffer entirely outside that lock. This is the kernel's
  first syscall that writes through a caller-supplied pointer rather
  than only register-passed words or a kernel-chosen address
  (`docs/adr/0003` notes "no user-pointer validation anywhere in this
  kernel yet" — true until this one) — every page the destination
  range touches is validated present/writable/user-accessible in the
  *caller's own* address space (a new `memory::virt::translate_in`,
  walking an arbitrary `pml4_frame`'s own page tables rather than the
  global kernel-only mapper every other memory helper uses) before the
  device is ever touched, and the actual copy goes through each page's
  own physical/HHDM alias rather than a raw write through the caller's
  virtual pointer — deliberately not leaning on "`SYSCALL` never
  switches `CR3`" (true on this kernel today, not a fact this function
  needs to depend on).
- **PCI enumeration + driver bring-up made unconditional**: `driver::virtio_blk::init()`
  now runs on every boot (previously only under the `block-driver-test`
  feature), the same way `driver::uart::init()` always has — `Err` just
  means no disk is attached, never a failure. The `PCI_ENUM_OK`/`FAIL`
  diagnostic lines moved from `main.rs`'s own test-only boot block into
  `virtio_blk::init()` itself (same marker text, so `test-block-driver`'s
  existing assertions needed no change) — this also removed a redundant
  second `find_device` scan Phase 2/3's own separate diagnostic block
  had been doing. `arch::x86_64::pci` and `driver::block`/`driver::virtio_blk`'s
  temporary `#[allow(dead_code)]` attributes all came out, exactly as
  each one's own Phase 2/3 doc comment said they would once this
  happened. Empirically confirmed this unconditional scan (up to 8192
  device-slot checks, worst case) doesn't meaningfully affect boot
  timing: the full `test-all` suite's existing timeouts (as tight as
  5s) all still pass reliably with every scenario now paying this cost.
- **New `block-syscall-test` feature + `xtask test-block-syscall`
  scenario** (wired into `test-all`/CI): a dummy ring-3 process with
  `BLOCK_CAP`/`CONSOLE_CAP` seeded directly into its own (otherwise
  empty) capability table — bypassing `init`/`SYS_GRANT` entirely, the
  same "throwaway dummy process, capabilities seeded directly" pattern
  every other kernel-feature-only test here already uses — issues a
  real `SYS_BLOCK_READ` via raw inline asm and reports whether the
  content matches. Passed on the first real boot attempt after fixing
  the finding below; confirmed reliable across three repeated runs.
- **A real, adversarial-testing-relevant bug found and fixed while
  writing that dummy process**: the first version declared its 512-byte
  read buffer as `let mut buf = [0u8; 512]` and used a byte-string
  literal inside a runtime `if`/`else` to pick the report message. Both
  faulted the process (`page fault ... USER_MODE`) on the very first
  boot attempt — in this codebase's own default *unoptimized* debug
  build (what every `xtask` scenario actually builds), a stack-array
  zero-initialization this size lowers to a real `memset` call, and a
  byte-string literal evaluated only inside a runtime branch isn't
  guaranteed constant-folded into an immediate the way a top-level
  `const` is. Both are calls/data-references that jump outside the
  two pages `Process::new_dummy` copies this function's own compiled
  code into — into whatever kernel code or data happens to sit next to
  it in memory, exactly the "no Rust-level function calls" constraint
  `milestone8_tests.rs`'s own doc comment already warns about, just not
  previously triggered by any *existing* dummy process (none of them
  needed a stack-local buffer this size before). Fixed by using
  `MaybeUninit<[u8; 512]>` (no zero-init codegen at all — nothing here
  needs the buffer's initial content, since `SYS_BLOCK_READ` overwrites
  it before it's ever read) and by making both possible report messages
  top-level `const`s selected by a runtime `if` between two
  already-fully-evaluated immediates, rather than evaluating either
  literal itself at runtime. Worth recording here since it's a genuine,
  previously-latent gap in how safely this project's own
  dummy-process-testing technique composes with a *real* local buffer,
  not just a fix local to this one test.
- **Zero warnings in all three configurations**: default,
  `--features block-driver-test`, and `--features block-syscall-test`
  all build and clippy (`-D warnings`) clean, as does `tarnos-abi`/
  `tarnos-kcore` (`--all-targets`, all 33 host tests plus proptests
  still passing after `Rights`/`SyscallError` grew new
  variants — `captable`'s own `rights_strategy` proptest generator
  widened from 2 bits to 3 accordingly) and the cross-compiled
  `tarnos-rt`/userland targets. Full `test-all` (24/24, including both
  new scenarios) passed cleanly with exit code 0.

### Phase 5 — Userland fixture and adversarial testing

A new userland test fixture (matching the one-purpose-per-fixture
convention — `echo-child` does IPC, `heap-child` does heap growth; this
one reads a known sector and reports pass/fail), plus `xtask` changes:
build and attach a disk image with known test data at known sectors,
and new `test-*` scenarios covering the happy path and at least: a
read past the end of the device, a read without holding the
capability, and a misaligned or otherwise invalid request — the same
"prove the boundary is enforced, not just unexercised" bar every prior
milestone's own boundary tests already hold to.

**Exit condition:** new scenarios pass, wired into `test-all` and CI;
full existing regression suite still green.

### Phase 6 — Documentation and sign-off

Write the ADR recording this milestone's decisions and findings
(matching every prior milestone's closing pattern), update
`docs/KNOWN-ISSUES.md` if anything new surfaces, and confirm this
milestone's own scope didn't disturb the still-open cross-core
corruption bug's own containment (`test-kitchen-sink` still excluded,
still runnable standalone).

**Exit condition:** ADR written; full regression green from a clean
build; both boot paths (BIOS/UEFI) still verified now that a disk
device is attached to the QEMU invocation for every scenario, not just
the new ones.

## Definition of Done

A process can issue a syscall naming a capability-gated block device,
request N sectors by LBA, and receive their real, correct contents
back — proven by an adversarial test suite covering the happy path and
real boundary violations, with the driver itself found via genuine PCI
enumeration rather than a hardcoded address, all on top of the
unmodified Milestone 10 base.

## What happens after

Milestone 12 builds a minimal, read-only filesystem on top of this
driver (a concrete on-disk format still to be chosen — most likely
FAT, for the same reason most hobby kernels reach for it: it's
simple, extremely well documented, and every existing OS can already
write one for building the test disk image), adds file open/read
syscalls, and wires `SYS_SPAWN` to load a program from the filesystem
instead of only from Limine's boot modules. That milestone's own plan
should be written once this one's driver is real and tested, not
before.
