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

/// The kernel object a capability slot refers to. One variant today —
/// deliberately an enum (not a bare `Arc<Endpoint>` field) so that memory
/// and IRQ capabilities can be added later without changing the syscall
/// ABI shape or `CapabilitySlot`'s layout.
#[derive(Clone)]
pub enum KernelObjectRef {
    Endpoint(Arc<Endpoint>),
}

pub type CapabilitySlot = tarnos_kcore::captable::CapabilitySlot<KernelObjectRef>;
pub type CapTable = tarnos_kcore::captable::CapTable<KernelObjectRef>;
