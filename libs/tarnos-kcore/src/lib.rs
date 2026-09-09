//! Hardware-independent kernel-internal data structures, split out of
//! `tarnos-kernel` specifically so they can be unit-tested on the host
//! with plain `cargo test -p tarnos-kcore` — no custom target, no QEMU.
//!
//! `#![cfg_attr(not(test), no_std)]`, not a bare `#![no_std]`: in normal
//! (non-test) builds — including every build the kernel itself links
//! against — this crate is `no_std`, exactly as strict as anything else
//! the kernel depends on. Under `cargo test` it compiles as an ordinary
//! `std` crate instead, which is what lets the standard `#[test]`
//! harness run at all; nothing in here actually *needs* `std` even in
//! tests, this attribute just avoids fighting the test harness for no
//! reason.
//!
//! Nothing here may depend on `x86_64`, `pic8259`, or any other
//! hardware-facing crate — that boundary is what makes host-side testing
//! possible in the first place. Kernel-specific glue (locking,
//! `PhysFrame`/`VirtAddr` types, interrupt safety) stays in
//! `tarnos-kernel` as a thin adapter over what's exported here.
#![cfg_attr(not(test), no_std)]

pub mod bitmap;
pub mod captable;
pub mod endpoint;
pub mod ring;

pub use bitmap::Bitmap;
pub use captable::{CapTable, CapabilitySlot, Rights};
pub use endpoint::{RecvOutcome, SendOutcome, Slot, Waiter};
pub use ring::RingBuffer;
