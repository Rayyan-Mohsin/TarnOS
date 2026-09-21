//! Capabilities: the unit of authority for naming kernel objects.
//!
//! A process can only address an object (an IPC endpoint today; memory or
//! IRQ objects later) that the kernel has explicitly placed into one of
//! its own capability table slots — there is no global, guessable, or
//! forgeable namespace of object IDs. See
//! `docs/adr/0001-microkernel-boundary-and-capabilities.md`.
//!
//! `CapTable`/`CapabilitySlot`/`Rights` themselves are
//! `tarnos_kcore::captable` types instantiated with [`KernelObjectRef`] —
//! the actual sparse-array and rights-checking logic lives there instead
//! of here specifically so it's unit-testable on the host without this
//! module's `Arc<Endpoint>` (which drags in hardware code transitively
//! through `Endpoint`'s locking).
use alloc::sync::Arc;

use super::endpoint::Endpoint;

pub use tarnos_kcore::captable::Rights;

/// The kernel object a capability slot refers to. Deliberately an enum
/// (not a bare `Arc<Endpoint>` field) so that further object kinds can
/// be added without changing the syscall ABI shape or `CapabilitySlot`'s
/// layout — `BlockDevice` (Milestone 11) is the first of those.
#[derive(Clone)]
pub enum KernelObjectRef {
    Endpoint(Arc<Endpoint>),
    /// The one virtio-blk device this milestone builds — a pure marker,
    /// not a handle carrying its own state: there is exactly one such
    /// device, reached through `driver::virtio_blk::with_device`'s own
    /// singleton, so a capability slot naming it needs nothing beyond
    /// "this slot may address the block device," which
    /// [`Rights::READ`](tarnos_abi::Rights::READ) alone doesn't already
    /// say (a slot's rights gate *what* is permitted; this variant is
    /// what says *which object*).
    ///
    /// `#[allow(dead_code)]`: only constructed today by `main.rs`'s
    /// `block-syscall-test`-gated boot block (Phase 5 wires it into the
    /// real `init`/`SYS_GRANT` path a normal build actually takes) — a
    /// default build never builds one, only matches against the
    /// possibility in `arch::x86_64::syscall::sys_block_read`.
    #[allow(dead_code)]
    BlockDevice,
}

pub type CapabilitySlot = tarnos_kcore::captable::CapabilitySlot<KernelObjectRef>;
pub type CapTable = tarnos_kcore::captable::CapTable<KernelObjectRef>;
