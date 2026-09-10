//! A process's capability table: the sparse, per-owner array a `CapIndex`
//! is looked up against, plus the `Rights` bitflags a slot carries.
//!
//! Generic over `T` (the kernel object a slot refers to) so this can be
//! unit-tested here without pulling in `tarnos-kernel`'s actual
//! `KernelObjectRef` — which itself only drags in hardware-facing code
//! transitively, through `Endpoint`'s locking. `tarnos-kernel::ipc::capability`
//! instantiates `CapTable<KernelObjectRef>` as a thin type alias over
//! what's here.
extern crate alloc;

use alloc::vec::Vec;

use tarnos_abi::{CapIndex, SyscallError};

/// `Rights` is a wire type shared between kernel and userland (a process
/// must be able to *express* the rights it's requesting in `sys_grant`),
/// so it's defined once in `tarnos-abi` alongside `Message`/`SyscallError`
/// rather than here.
pub use tarnos_abi::Rights;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilitySlot<T> {
    pub object: T,
    pub rights: Rights,
}

/// A sparse array indexed by [`CapIndex`]. Growable (an empty table costs
/// nothing), but never shrinks — slots are cleared in place rather than
/// compacted, so an index always names the same slot for the table's
/// lifetime.
pub struct CapTable<T> {
    slots: Vec<Option<CapabilitySlot<T>>>,
}

impl<T> CapTable<T> {
    pub fn new() -> Self {
        Self { slots: Vec::new() }
    }

    pub fn insert(&mut self, index: CapIndex, slot: CapabilitySlot<T>) {
        let i = index.0 as usize;
        if i >= self.slots.len() {
            self.slots.resize_with(i + 1, || None);
        }
        self.slots[i] = Some(slot);
    }

    /// Looks up a slot, without any rights check — use [`CapTable::lookup`]
    /// when the caller needs a specific right, which is every syscall path.
    pub fn get(&self, index: CapIndex) -> Result<&CapabilitySlot<T>, SyscallError> {
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
    ) -> Result<&CapabilitySlot<T>, SyscallError> {
        let slot = self.get(index)?;
        if slot.rights.contains(required) {
            Ok(slot)
        } else {
            Err(SyscallError::PermissionDenied)
        }
    }
}

impl<T> Default for CapTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{CapTable, CapabilitySlot, Rights};
    use tarnos_abi::{CapIndex, SyscallError};

    #[test]
    fn lookup_on_empty_table_is_bad_capability() {
        let table: CapTable<u32> = CapTable::new();
        assert_eq!(
            table.lookup(CapIndex(0), Rights::SEND),
            Err(SyscallError::BadCapability)
        );
    }

    #[test]
    fn get_bypasses_rights_check() {
        let mut table = CapTable::new();
        table.insert(
            CapIndex(3),
            CapabilitySlot {
                object: 42u32,
                rights: Rights::empty(),
            },
        );
        assert_eq!(table.get(CapIndex(3)).unwrap().object, 42);
    }

    #[test]
    fn lookup_grants_only_held_rights() {
        let mut table = CapTable::new();
        table.insert(
            CapIndex(0),
            CapabilitySlot {
                object: (),
                rights: Rights::SEND,
            },
        );
        assert!(table.lookup(CapIndex(0), Rights::SEND).is_ok());
        assert_eq!(
            table.lookup(CapIndex(0), Rights::RECV),
            Err(SyscallError::PermissionDenied)
        );
        assert_eq!(
            table.lookup(CapIndex(0), Rights::SEND | Rights::RECV),
            Err(SyscallError::PermissionDenied)
        );
    }

    #[test]
    fn unknown_index_is_bad_capability_not_out_of_bounds_panic() {
        let mut table = CapTable::new();
        table.insert(
            CapIndex(5),
            CapabilitySlot {
                object: (),
                rights: Rights::SEND | Rights::RECV,
            },
        );
        // Indices below, between, and above the one populated slot must
        // all report "no such capability" — including ones the sparse
        // Vec never even grew to cover.
        assert_eq!(table.get(CapIndex(0)), Err(SyscallError::BadCapability));
        assert_eq!(table.get(CapIndex(4)), Err(SyscallError::BadCapability));
        assert_eq!(table.get(CapIndex(99)), Err(SyscallError::BadCapability));
        assert!(table.get(CapIndex(5)).is_ok());
    }

    #[test]
    fn reinserting_at_the_same_index_replaces_the_slot() {
        let mut table = CapTable::new();
        table.insert(
            CapIndex(0),
            CapabilitySlot {
                object: 1u32,
                rights: Rights::SEND,
            },
        );
        table.insert(
            CapIndex(0),
            CapabilitySlot {
                object: 2u32,
                rights: Rights::RECV,
            },
        );
        let slot = table.get(CapIndex(0)).unwrap();
        assert_eq!(slot.object, 2);
        assert_eq!(slot.rights, Rights::RECV);
    }
}
