//! Capabilities: the unit of authority for naming kernel objects.
//!
//! A process can only address an object (an IPC endpoint today; memory or
//! IRQ objects later) that the kernel has explicitly placed into one of
//! its own capability table slots — there is no global, guessable, or
//! forgeable namespace of object IDs. See
//! `docs/adr/0001-microkernel-boundary-and-capabilities.md`.
use alloc::sync::Arc;
use alloc::vec::Vec;

use tarnos_abi::{CapIndex, SyscallError};

use super::endpoint::Endpoint;

/// The kernel object a capability slot refers to. One variant today —
/// deliberately an enum (not a bare `Arc<Endpoint>` field) so that memory
/// and IRQ capabilities can be added later without changing the syscall
/// ABI shape or `CapabilitySlot`'s layout.
#[derive(Clone)]
pub enum KernelObjectRef {
    Endpoint(Arc<Endpoint>),
}

bitflags::bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Rights: u8 {
        const SEND = 0b01;
        const RECV = 0b10;
    }
}

#[derive(Clone)]
pub struct CapabilitySlot {
    pub object: KernelObjectRef,
    pub rights: Rights,
}

/// A process's capability table: a sparse array indexed by [`CapIndex`].
/// Growable (an empty table costs nothing), but never shrinks — slots are
/// cleared in place rather than compacted, so an index always names the
/// same slot for the table's lifetime.
pub struct CapTable {
    slots: Vec<Option<CapabilitySlot>>,
}

impl CapTable {
    pub fn new() -> Self {
        Self { slots: Vec::new() }
    }

    pub fn insert(&mut self, index: CapIndex, slot: CapabilitySlot) {
        let i = index.0 as usize;
        if i >= self.slots.len() {
            self.slots.resize_with(i + 1, || None);
        }
        self.slots[i] = Some(slot);
    }

    /// Looks up a slot, without any rights check — use [`CapTable::lookup`]
    /// when the caller needs a specific right, which is every syscall path.
    pub fn get(&self, index: CapIndex) -> Result<&CapabilitySlot, SyscallError> {
        self.slots
            .get(index.0 as usize)
            .and_then(|slot| slot.as_ref())
            .ok_or(SyscallError::BadCapability)
    }

    /// Looks up a slot and checks it carries every right in `required`.
    pub fn lookup(
        &self,
        index: CapIndex,
        required: Rights,
    ) -> Result<&CapabilitySlot, SyscallError> {
        let slot = self.get(index)?;
        if slot.rights.contains(required) {
            Ok(slot)
        } else {
            Err(SyscallError::PermissionDenied)
        }
    }
}

impl Default for CapTable {
    fn default() -> Self {
        Self::new()
    }
}
