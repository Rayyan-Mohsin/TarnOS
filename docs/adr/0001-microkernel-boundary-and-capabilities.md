# 0001: Microkernel Boundary and Capabilities

## Status

Accepted. Implemented in `kernel/src/ipc/capability.rs`, `kernel/src/task/process.rs`.

## Context

The top-listed priority for TarnOS is microkernel architecture and
isolation. That's a design commitment, not just a folder layout: it has
to show up in what the kernel refuses to let a process do, not just in
where the code for a driver happens to live.

Two boundaries needed deciding before any code could be written against
them: what runs in ring 0 versus ring 3, and how a ring-3 process is
allowed to name and address anything outside its own address space.

## Decision

**Kernel/user split.** Everything in `kernel/` runs at ring 0 with full
hardware access. Everything else — `userland/init` today, any future
program — runs at ring 3 in its own address space (`memory::virt::AddressSpace`,
a separate PML4 with only the kernel half shared) and can only affect the
rest of the system through a syscall trap. This milestone still runs the
UART driver in-kernel (see ADR 0003 in the driver section below, actually
covered by the driver framework's own doc comments) because boot
diagnostics need it before process infrastructure exists — but nothing
about the syscall/IPC boundary assumes that. A driver moved to its own
process later is additive, not a rewrite.

**Capabilities, not global names.** A process cannot address a kernel
object — an IPC endpoint today, memory or IRQ objects later — by a global
ID, handle table shared across processes, or pointer. It can only name
what the kernel has explicitly placed into one of *its own* capability
table's slots (`ipc::CapTable`, indexed by `tarnos_abi::CapIndex`). Every
syscall that touches a kernel object takes a `CapIndex`, and the kernel
looks it up in the calling process's own table — never anyone else's.
This is the seL4/L4 model, not the Unix model of `/dev` nodes plus
permission bits: there is no namespace to enumerate or guess into, only
what you were handed.

`CapabilitySlot { object: KernelObjectRef, rights: Rights }` pairs an
object reference with a `Rights` bitflag (`SEND`, `RECV` today). A slot
grants exactly the operations its rights allow — a process holding a
`CONSOLE_CAP` with only `SEND` cannot use it to receive, even though it
names the same endpoint a receiver elsewhere holds with `RECV`. Rights
attach to the *slot*, not the object, so the same underlying `Endpoint`
can be hand out with different capabilities to different holders.

`KernelObjectRef` is an enum with one variant today (`Endpoint`)
specifically so memory and IRQ capabilities can be added as new variants
later without changing `CapabilitySlot`'s layout or the syscall ABI shape
that carries a bare `CapIndex` across the trap boundary.

**Seeded, not negotiated, initial capabilities.** `init` is not given a
way to ask for capabilities at runtime this milestone — it is seeded at
process-creation time with slot `CONSOLE_CAP` (index 0) already holding
`SEND` rights to the console server's endpoint, mirroring seL4's
kernel-populated initial CSpace for the root task. A future capability
grant/transfer syscall (letting one process hand a capability to another,
e.g. as part of `send`) is the natural extension point once more than one
capability-bearing object exists to hand around — deliberately out of
scope this milestone, since the only two parties that need to communicate
(`init` and the console server) can both be wired up entirely by boot
code before either runs.

## Consequences

- Every future syscall that touches a kernel object takes the same shape:
  a `CapIndex`, looked up and rights-checked against the calling
  process's own table (`CapTable::lookup`) before anything else happens.
  There is no separate code path for "trusted" callers to skip this.
- A compromised or buggy process can only ever misuse what it already
  holds a capability for — it has no way to reach an object it was never
  given, because there is no global address space of objects to reach
  into.
- Adding a new kind of kernel object (memory grant, IRQ, a second
  endpoint) means adding a `KernelObjectRef` variant and a `Rights` bit,
  not touching the trap trampoline or any existing syscall's argument
  shape.
- The lack of a runtime grant/transfer syscall is a real, current
  limitation, not an oversight: it means every process's full set of
  reachable objects must be decided at creation time by boot code. This
  is fine for a single seeded `init`, but blocks any future scenario
  where processes need to be handed new capabilities dynamically (e.g.
  spawning a child and handing it one specific endpoint) — that syscall
  is deferred, explicitly, to whenever a milestone actually needs it.
  **Update (milestone 3):** closed. See
  `docs/adr/0006-dynamic-process-creation-and-capability-transfer.md` —
  `SYS_SPAWN`/`SYS_GRANT`/`SYS_PROCESS_START` let a running process
  create a child and hand it capabilities of its own choosing, narrowed
  to rights it itself holds.
