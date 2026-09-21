//! `BlockDevice`: an addressed, whole-sector-I/O device trait for
//! storage -- the `CharDevice`-equivalent for block devices, designed
//! from scratch (Milestone 10 Phase 7 found nothing existing to reuse:
//! `CharDevice` is byte-at-a-time with no addressing) and shaped by
//! exactly the one real driver built against it this milestone
//! (`super::virtio_blk`), not speculatively generalized for a second,
//! hypothetical device that doesn't exist yet.
//!
//! `read_sectors` is called unconditionally since Milestone 11 Phase 4
//! (`arch::x86_64::syscall::sys_block_read`) — no dead-code allowance
//! needed for the trait itself. `capacity_sectors` still carries its own
//! (see that method's doc comment for why).
use super::Driver;

/// Every sector this milestone's one real device (and, in practice,
/// every other block device this trait is likely to ever describe --
/// SATA/NVMe/virtio-blk all use it) is addressed in. Fixed, not a
/// per-device property, because nothing here needs it to vary.
pub const SECTOR_SIZE: usize = 512;

/// What can go wrong servicing a [`BlockDevice::read_sectors`] call --
/// deliberately small: exactly the failure modes this milestone's one
/// real driver and its own adversarial tests (Phase 5) need to
/// distinguish, not a speculative catch-all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    /// `buf` is empty, or its length isn't a whole multiple of
    /// [`SECTOR_SIZE`].
    BufferNotSectorAligned,
    /// The requested read (`lba..lba + buf.len() / SECTOR_SIZE`) would
    /// read at or past [`BlockDevice::capacity_sectors`].
    OutOfRange,
    /// More sectors were requested in one call than this driver's fixed
    /// internal per-request transfer limit supports — see
    /// `virtio_blk::MAX_SECTORS_PER_REQUEST`.
    RequestTooLarge,
    /// The device itself reported failure completing the request.
    DeviceError,
}

/// An addressed, whole-sector-I/O block device — read-only this
/// milestone (see the milestone's own Non-goals: no writes yet).
pub trait BlockDevice: Driver {
    /// Total number of [`SECTOR_SIZE`]-byte sectors this device holds.
    /// `virtio_blk::VirtioBlk` enforces its own bound internally (via its
    /// own private field, not this method) inside `read_sectors` today —
    /// this accessor exists for Phase 4/5's own consumers (a real
    /// syscall's own bounds check, and a boundary test that wants the
    /// device's own reported capacity rather than hardcoding what
    /// `xtask`'s test disk image happens to be sized) that don't exist
    /// yet.
    #[allow(dead_code)]
    fn capacity_sectors(&self) -> u64;

    /// Reads `buf.len() / SECTOR_SIZE` whole sectors starting at `lba`
    /// into `buf`. On any `Err`, `buf` is left in an unspecified state
    /// (never partially filled and trusted — a caller must not read `buf`
    /// after an error).
    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError>;
}
