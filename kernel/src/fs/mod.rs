//! Filesystem support -- read-only, FAT12 only, flat root directory
//! only, exactly matching `docs/MILESTONE-12-FILESYSTEM.md`'s own
//! Decisions and Non-goals. One implementation over one already-proven
//! block device (`driver::block::BlockDevice`), not a general
//! virtual-filesystem layer -- there is no second filesystem format or
//! second device for one to abstract over yet.
pub mod fat;
