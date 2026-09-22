//! FAT12 parsing: boot sector/BPB, FAT table cluster-chain traversal,
//! flat root-directory 8.3 lookup, and whole-file reads -- built
//! entirely on top of `driver::block::BlockDevice`, doing its own LBA
//! math and never reaching into a driver's own internals, matching
//! this milestone's own Decisions.
//!
//! Every field offset and formula here was confirmed against a real
//! `mformat`/`mcopy`-built 1.44 MiB image, not copied from the FAT
//! spec alone -- see `docs/MILESTONE-12-FILESYSTEM.md`'s own Phase 1
//! Findings for the full byte-level verification this module's
//! constants and formulas are drawn from.
//!
//! Read-only (this milestone's own Non-goal: no writes), FAT12 only
//! (rejected at [`Fat12Volume::mount`] via the spec's own authoritative
//! cluster-count rule, never the informational `FilSysType` label
//! string), no long filenames (8.3 only), no subdirectories (the root
//! directory is the only directory this module ever reads), and no
//! caching -- every [`Fat12Volume::read_file`] call re-reads the FAT
//! table and every data cluster fresh from `device`.
//!
//! [`mount_root`]/[`with_root`] are this module's own singleton, one
//! mounted volume at most, reached the same way
//! `driver::virtio_blk::{init, with_device}` reach their own one real
//! device -- unconditional, real boot-sequence code as of Milestone 12
//! Phase 3 (`main.rs` calls `mount_root` right before spawning `init`),
//! not gated behind any test feature.
extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use crate::driver::block::{BlockDevice, BlockError, SECTOR_SIZE};
use crate::driver::virtio_blk::{self, VirtioBlk};
use crate::sync::SpinLock;

/// Boot sector signature bytes' own fixed offset and required value
/// (every FAT boot sector, of any variant, ends this way).
const BOOT_SIG_OFFSET: usize = 510;
const BOOT_SIG: [u8; 2] = [0x55, 0xAA];

// BPB field offsets (FAT spec, common to FAT12/16/32) -- confirmed
// against a real image in this milestone's own Phase 1 findings.
const BPB_BYTS_PER_SEC: usize = 11;
const BPB_SEC_PER_CLUS: usize = 13;
const BPB_RSVD_SEC_CNT: usize = 14;
const BPB_NUM_FATS: usize = 16;
const BPB_ROOT_ENT_CNT: usize = 17;
const BPB_TOT_SEC_16: usize = 19;
const BPB_FAT_SZ_16: usize = 22;
const BPB_TOT_SEC_32: usize = 32;

/// One 32-byte root-directory entry's own field offsets, relative to
/// the start of that entry -- confirmed field-by-field in this
/// milestone's own Phase 1 findings.
const DIRENT_SIZE: usize = 32;
const DIRENT_NAME: usize = 0;
const DIRENT_NAME_LEN: usize = 11;
const DIRENT_ATTR: usize = 11;
const DIRENT_FST_CLUS_LO: usize = 26;
const DIRENT_FILE_SIZE: usize = 28;

/// Directory-entry first-byte markers (FAT spec).
const DIRENT_FREE_REST: u8 = 0x00;
const DIRENT_DELETED: u8 = 0xE5;

const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_LONG_NAME_MASK: u8 = 0x0F;

/// FAT12 end-of-chain markers span `0xFF8..=0xFFF`; `0xFF7` marks a bad
/// cluster; `0x000` marks a free cluster -- none of the latter two are
/// ever valid to see while following a real file's own chain.
const FAT12_EOC_MIN: u16 = 0xFF8;
const FAT12_BAD_CLUSTER: u16 = 0xFF7;

/// FAT12 vs. FAT16's own authoritative dividing line (the spec's own
/// rule, computed from geometry -- never read from the informational
/// `FilSysType` label string). See this milestone's Phase 1 findings
/// for the worked example this threshold was confirmed against.
const FAT12_MAX_CLUSTERS: u64 = 4085;

/// The first two FAT entries are always reserved (never a real
/// cluster); every real file's cluster chain starts at 2.
const FIRST_DATA_CLUSTER: u32 = 2;

fn read_u16(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(data[off..off + 2].try_into().unwrap())
}
fn read_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatError {
    /// The underlying block device failed to service a read.
    Io(BlockError),
    /// Boot sector is missing the mandatory `55 AA` signature.
    BadSignature,
    /// The BPB claims a sector size other than [`SECTOR_SIZE`] -- every
    /// LBA/offset formula in this module assumes they match, and no
    /// image this milestone's own tooling builds ever disagrees.
    UnsupportedSectorSize,
    /// A BPB field this module divides or multiplies by was zero.
    CorruptBootSector,
    /// The volume's own computed cluster count doesn't fall in FAT12's
    /// range -- this milestone only implements FAT12 (see the module
    /// doc comment).
    NotFat12,
    /// No root-directory entry matched the requested name.
    NoSuchFile,
    /// A cluster-chain walk read an entry outside the FAT table's own
    /// bounds, or the chain exceeded the volume's own total cluster
    /// count without reaching an end-of-chain marker (a cycle, or a
    /// chain that runs into a free/bad/reserved entry mid-file).
    CorruptFat,
    /// The requested name doesn't fit the 8.3 shape this milestone
    /// supports (see the module doc comment: no long filenames).
    NameTooLong,
    /// [`mount_root`] found no block device at all this boot -- an
    /// ordinary outcome (most scenarios attach no disk), never a bug.
    NoDevice,
}

/// A mounted FAT12 volume's own parsed geometry -- everything
/// [`read_file`](Fat12Volume::read_file) needs to turn a file name into
/// real LBAs, computed once at [`mount`](Fat12Volume::mount) time from
/// the boot sector's BPB.
pub struct Fat12Volume {
    sectors_per_cluster: u64,
    fat_size_sectors: u64,
    fat_start_lba: u64,
    root_dir_start_lba: u64,
    root_dir_sectors: u64,
    data_start_lba: u64,
    /// Total data-region cluster count -- both this volume's own FAT12
    /// confirmation (via [`FAT12_MAX_CLUSTERS`]) and the upper bound a
    /// cluster-chain walk is allowed to run for before being treated as
    /// corrupt (see [`Fat12Volume::cluster_chain`]).
    total_clusters: u64,
}

impl Fat12Volume {
    /// Reads and parses `device`'s boot sector, confirming (via the
    /// spec's own cluster-count rule, not the informational
    /// `FilSysType` label) that it is FAT12 before returning a volume
    /// any other method here will trust.
    pub fn mount<D: BlockDevice + ?Sized>(device: &mut D) -> Result<Self, FatError> {
        let mut boot_sector = [0u8; SECTOR_SIZE];
        device
            .read_sectors(0, &mut boot_sector)
            .map_err(FatError::Io)?;

        if boot_sector[BOOT_SIG_OFFSET..BOOT_SIG_OFFSET + 2] != BOOT_SIG {
            return Err(FatError::BadSignature);
        }

        let bytes_per_sector = read_u16(&boot_sector, BPB_BYTS_PER_SEC);
        if bytes_per_sector as usize != SECTOR_SIZE {
            return Err(FatError::UnsupportedSectorSize);
        }

        let sectors_per_cluster = boot_sector[BPB_SEC_PER_CLUS] as u64;
        let reserved_sector_count = read_u16(&boot_sector, BPB_RSVD_SEC_CNT) as u64;
        let num_fats = boot_sector[BPB_NUM_FATS] as u64;
        let root_entry_count = read_u16(&boot_sector, BPB_ROOT_ENT_CNT) as u64;
        let total_sectors_16 = read_u16(&boot_sector, BPB_TOT_SEC_16) as u64;
        let fat_size_sectors = read_u16(&boot_sector, BPB_FAT_SZ_16) as u64;
        let total_sectors_32 = read_u32(&boot_sector, BPB_TOT_SEC_32) as u64;

        if sectors_per_cluster == 0 || num_fats == 0 || fat_size_sectors == 0 {
            return Err(FatError::CorruptBootSector);
        }

        // Root directory occupies a whole number of sectors -- 32-byte
        // entries never straddle a sector boundary on a real FAT
        // volume, so this division is always exact in practice, but
        // rounds up defensively rather than assuming it.
        let root_dir_sectors =
            (root_entry_count * DIRENT_SIZE as u64).div_ceil(bytes_per_sector as u64);
        let fat_start_lba = reserved_sector_count;
        let root_dir_start_lba = fat_start_lba + num_fats * fat_size_sectors;
        let data_start_lba = root_dir_start_lba + root_dir_sectors;

        let total_sectors = if total_sectors_16 != 0 {
            total_sectors_16
        } else {
            total_sectors_32
        };
        let data_sectors = total_sectors.saturating_sub(data_start_lba);
        let total_clusters = data_sectors / sectors_per_cluster;

        if total_clusters >= FAT12_MAX_CLUSTERS {
            return Err(FatError::NotFat12);
        }

        Ok(Self {
            sectors_per_cluster,
            fat_size_sectors,
            fat_start_lba,
            root_dir_start_lba,
            root_dir_sectors,
            data_start_lba,
            total_clusters,
        })
    }

    /// Reads this volume's first FAT copy (never the second -- see the
    /// module doc comment) into a freshly allocated buffer, one sector
    /// at a time -- a `BlockDevice` is only ever guaranteed to service a
    /// single-sector read (`virtio_blk`'s own `MAX_SECTORS_PER_REQUEST`
    /// is a driver-internal limit this module deliberately never reaches
    /// past `BlockDevice` to see), so this never assumes a multi-sector
    /// read of a whole (potentially several-KiB) FAT table is safe in
    /// one call.
    fn read_fat_table<D: BlockDevice + ?Sized>(&self, device: &mut D) -> Result<Vec<u8>, FatError> {
        let mut buf = vec![0u8; (self.fat_size_sectors * SECTOR_SIZE as u64) as usize];
        for i in 0..self.fat_size_sectors {
            let start = (i as usize) * SECTOR_SIZE;
            device
                .read_sectors(self.fat_start_lba + i, &mut buf[start..start + SECTOR_SIZE])
                .map_err(FatError::Io)?;
        }
        Ok(buf)
    }

    /// Decodes FAT12's packed 12-bit entry for `cluster` out of an
    /// already-read FAT table buffer, using the standard
    /// `FatOffset = N*3/2`, even/odd-nibble-masking formula -- see this
    /// milestone's Phase 1 findings for this formula confirmed against
    /// two real cluster chains.
    fn fat12_entry(fat: &[u8], cluster: u32) -> Result<u16, FatError> {
        let offset = (cluster as usize) * 3 / 2;
        if offset + 1 >= fat.len() {
            return Err(FatError::CorruptFat);
        }
        let packed = u16::from_le_bytes([fat[offset], fat[offset + 1]]);
        Ok(if cluster.is_multiple_of(2) {
            packed & 0x0FFF
        } else {
            packed >> 4
        })
    }

    /// Walks `fat` starting at `start_cluster`, returning every cluster
    /// in the chain in order. Bounded by `self.total_clusters` so a
    /// cyclic or otherwise-corrupt chain can never loop forever --
    /// nothing about a block device's own content is trusted more than
    /// that.
    fn cluster_chain(&self, fat: &[u8], start_cluster: u32) -> Result<Vec<u32>, FatError> {
        let mut chain = Vec::new();
        let mut cluster = start_cluster;
        loop {
            if (cluster as u64) < FIRST_DATA_CLUSTER as u64 {
                return Err(FatError::CorruptFat);
            }
            chain.push(cluster);
            if chain.len() as u64 > self.total_clusters {
                return Err(FatError::CorruptFat);
            }
            let next = Self::fat12_entry(fat, cluster)?;
            if next >= FAT12_EOC_MIN {
                break;
            }
            if next == 0 || next == FAT12_BAD_CLUSTER {
                return Err(FatError::CorruptFat);
            }
            cluster = next as u32;
        }
        Ok(chain)
    }

    /// Converts an input name like `"HELLO.TXT"` into the packed
    /// 11-byte, space-padded, uppercase form a directory entry's own
    /// name field stores -- see this milestone's Phase 1 findings for
    /// this exact packed form confirmed against a real image
    /// (`"HELLO   TXT"`).
    fn to_83_name(name: &str) -> Option<[u8; DIRENT_NAME_LEN]> {
        let (base, ext) = match name.rsplit_once('.') {
            Some((b, e)) => (b, e),
            None => (name, ""),
        };
        if base.is_empty() || base.len() > 8 || ext.len() > 3 || !name.is_ascii() {
            return None;
        }
        let mut out = [b' '; DIRENT_NAME_LEN];
        for (i, c) in base.bytes().enumerate() {
            out[i] = c.to_ascii_uppercase();
        }
        for (i, c) in ext.bytes().enumerate() {
            out[8 + i] = c.to_ascii_uppercase();
        }
        Some(out)
    }

    /// Scans every root-directory entry for one matching `name83`,
    /// returning its `(first_cluster, file_size)`. Flat lookup only --
    /// this milestone's own Decisions rule out subdirectories, so the
    /// root directory is the only directory this ever reads.
    fn find_dir_entry<D: BlockDevice + ?Sized>(
        &self,
        device: &mut D,
        name83: &[u8; DIRENT_NAME_LEN],
    ) -> Result<(u16, u32), FatError> {
        let mut sector_buf = [0u8; SECTOR_SIZE];
        for i in 0..self.root_dir_sectors {
            device
                .read_sectors(self.root_dir_start_lba + i, &mut sector_buf)
                .map_err(FatError::Io)?;
            for entry in sector_buf.as_chunks::<DIRENT_SIZE>().0 {
                let first_byte = entry[DIRENT_NAME];
                if first_byte == DIRENT_FREE_REST {
                    // A free entry marks the end of the in-use portion
                    // of the directory (FAT spec) -- nothing after it
                    // is ever in use either.
                    return Err(FatError::NoSuchFile);
                }
                if first_byte == DIRENT_DELETED {
                    continue;
                }
                let attr = entry[DIRENT_ATTR];
                if attr & ATTR_LONG_NAME_MASK == ATTR_LONG_NAME_MASK {
                    continue; // long-filename entry -- not supported (module doc comment)
                }
                if attr & ATTR_VOLUME_ID != 0 {
                    continue; // the volume-label entry, not a file
                }
                if &entry[DIRENT_NAME..DIRENT_NAME + DIRENT_NAME_LEN] == name83 {
                    let first_cluster = read_u16(entry, DIRENT_FST_CLUS_LO);
                    let file_size = read_u32(entry, DIRENT_FILE_SIZE);
                    return Ok((first_cluster, file_size));
                }
            }
        }
        Err(FatError::NoSuchFile)
    }

    /// Reads a named file's full contents into a freshly allocated,
    /// exactly-`file_size`-byte buffer -- boot sector, FAT table, and
    /// every data cluster re-read from `device` fresh each call (this
    /// milestone's own Non-goal: no caching).
    pub fn read_file<D: BlockDevice + ?Sized>(
        &self,
        device: &mut D,
        name: &str,
    ) -> Result<Vec<u8>, FatError> {
        let name83 = Self::to_83_name(name).ok_or(FatError::NameTooLong)?;
        let (first_cluster, file_size) = self.find_dir_entry(device, &name83)?;
        if file_size == 0 {
            return Ok(Vec::new());
        }

        let fat = self.read_fat_table(device)?;
        let chain = self.cluster_chain(&fat, first_cluster as u32)?;

        let cluster_bytes = (self.sectors_per_cluster * SECTOR_SIZE as u64) as usize;
        let mut data = vec![0u8; chain.len() * cluster_bytes];
        for (i, &cluster) in chain.iter().enumerate() {
            let lba = self.data_start_lba
                + (cluster as u64 - FIRST_DATA_CLUSTER as u64) * self.sectors_per_cluster;
            let start = i * cluster_bytes;
            device
                .read_sectors(lba, &mut data[start..start + cluster_bytes])
                .map_err(FatError::Io)?;
        }

        data.truncate(file_size as usize);
        Ok(data)
    }
}

static ROOT_VOLUME: SpinLock<Option<Fat12Volume>> = SpinLock::new(None);

/// Attempts to mount the one filesystem this kernel supports off
/// whatever block device `virtio_blk::init` found this boot, storing it
/// for [`with_root`] on success. Called once, unconditionally, from the
/// real boot sequence right before `init` is spawned (`main.rs`) --
/// `Err` (no device this boot, or the device's own content isn't a
/// valid FAT12 volume) is an ordinary, expected outcome for most
/// scenarios, never a panic; the caller seeds `FS_CAP` into `init`'s own
/// capability table only when this returns `Ok`.
pub fn mount_root() -> Result<(), FatError> {
    let volume = virtio_blk::with_device(Fat12Volume::mount).ok_or(FatError::NoDevice)??;
    *ROOT_VOLUME.lock() = Some(volume);
    Ok(())
}

/// Runs `f` against the mounted root volume and the one real block
/// device, or `None` if [`mount_root`] was never called or found
/// nothing to mount. Mirrors `virtio_blk::with_device`'s own singleton
/// shape.
pub fn with_root<R>(f: impl FnOnce(&Fat12Volume, &mut VirtioBlk) -> R) -> Option<R> {
    let guard = ROOT_VOLUME.lock();
    let volume = guard.as_ref()?;
    virtio_blk::with_device(|device| f(volume, device))
}
