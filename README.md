# TarnOS

An original Rust microkernel for x86_64, boot-loaded via
[Limine](https://github.com/limine-bootloader/limine). Kernel/user
isolation, seL4/L4-style capability-based IPC (no global object
namespace — a process can only reach what it's been explicitly handed),
a preemptible cross-core process scheduler, and a small POSIX-adjacent
syscall ABI, built up milestone by milestone with an adversarial test
for every boundary it claims to enforce. See `docs/adr/` for the full,
numbered design history — every non-trivial decision here has a
corresponding ADR explaining why, not just what.

## Layout

- `kernel/` — the kernel itself (`tarnos-kernel`): boot, memory
  management, the scheduler, IPC, drivers, the syscall entry path.
- `libs/tarnos-abi` — the syscall ABI shared, unmodified, by both the
  kernel and every userland binary (syscall numbers, message format,
  error codes) — the wire contract that keeps the two sides from
  drifting apart.
- `libs/tarnos-kcore` — pure, host-testable logic extracted out of the
  kernel (ring buffer, bitmap, capability table, IPC endpoint slot
  machine) specifically so it can be unit- and property-tested without
  needing QEMU or a custom target.
- `libs/tarnos-rt` — the minimal usermode runtime every TarnOS-native
  binary links: an entry point, a panic handler, syscall wrappers, and
  a lazily-growing heap allocator.
- `userland/` — userland binaries: `init` (the first process) plus a
  handful of single-purpose test fixtures (`echo-child`,
  `exit-code-child`, `heap-child`).
- `xtask/` — build/ISO/QEMU automation and every integration test
  scenario (`cargo run -p xtask -- <command>`).
- `docs/adr/` — architecture decision records, numbered and dated,
  the permanent record of why the kernel is shaped the way it is.
- `docs/KNOWN-ISSUES.md` — the one open bug worth knowing about before
  building on top of this kernel; see below.

## Building and running

Requires the pinned nightly toolchain (`rust-toolchain.toml` selects it
automatically via `rustup`), plus `xorriso`, `qemu-system-x86`, and
OVMF firmware for UEFI boots (see `.github/workflows/ci.yml` for the
exact package names on Ubuntu).

```sh
cargo run -p xtask -- build   # cross-compile the kernel + every userland binary
cargo run -p xtask -- iso     # also assemble build/tarnos.iso
cargo run -p xtask -- run     # build (if needed) and boot it in QEMU
cargo run -p xtask -- run --uefi   # boot via OVMF instead of legacy BIOS
```

`xtask` builds the kernel and userland against custom `no_std` targets
(`x86_64-unknown-none`, `targets/x86_64-tarnos-user.json`) that a plain
`cargo build` at the workspace root doesn't target — always go through
`xtask`, not `cargo build` directly, for anything kernel- or
userland-facing.

## Testing

```sh
cargo run -p xtask -- test-all              # every integration scenario (~22), each boots a real QEMU instance
cargo test -p tarnos-kcore -p tarnos-abi    # host-side unit + property tests, no QEMU needed
cargo clippy -p tarnos-kcore -p tarnos-abi --all-targets -- -D warnings
```

Run `cargo run -p xtask -- ` with no arguments (or see `xtask/src/main.rs`'s
`print_usage`) for the full list of individual `test-*` scenarios — each
one boots a purpose-built kernel variant and asserts on its serial
output, documented inline with what it specifically checks.

One scenario, `test-kitchen-sink`, is deliberately **not** part of
`test-all`/CI — it's the reproduction for the open bug described below,
not a real workload. See `docs/KNOWN-ISSUES.md`.

## Known issues

`docs/KNOWN-ISSUES.md` documents one open bug: a cross-core scheduling
race, under active investigation since Milestone 9 (`docs/adr/0012`
through `docs/adr/0029`), that requires multiple cores and heavy
scheduling pressure to reproduce, fails safely (a clean panic, never
silent corruption), and does not affect any of `test-all`'s own
scenarios or ordinary use. Read that file before relying on this
kernel under heavy concurrent load, or before continuing that
investigation yourself.
