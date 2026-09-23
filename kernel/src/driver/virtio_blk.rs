//! virtio-blk driver: a synchronous (polled, not interrupt-driven),
//! read-only block driver for the virtio 1.0+ ("modern") PCI interface —
//! confirmed against this exact environment's QEMU before writing this,
//! not assumed from the spec alone (see
//! `docs/MILESTONE-11-BLOCK-STORAGE-DRIVER.md`'s own Phase 1 findings:
//! `virtio-blk-pci-non-transitional`, vendor `0x1AF4` device `0x1042`,
//! two MMIO BARs, no I/O-port interface at all, and the capability →
//! BAR/offset mapping deliberately *not* hardcoded from that
//! observation — read fresh from the device's own capability list
//! below).
//!
//! One virtqueue, one request in flight at a time — this milestone's
//! own Non-goals rule out interrupt-driven I/O and a general PCI driver
//! framework; a single synchronous poll-until-complete request is the
//! simplest correct thing that can prove real sector reads work end to
//! end, the same "simplest correct thing first" choice the original
//! UART driver made before this project ever built anything async.
//!
//! [`init`] is an unconditional part of boot (Milestone 11 Phase 4) —
//! called once regardless of which, if any, test feature is active, the
//! same way `driver::uart::init()` always runs. `Err` just means no
//! matching PCI device this boot (every scenario except the ones that
//! attach a disk); it is never itself a bug.
use core::sync::atomic::{fence, Ordering};

use x86_64::structures::paging::{FrameAllocator, Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

use super::block::{BlockDevice, BlockError, SECTOR_SIZE};
use super::Driver;
use crate::arch::x86_64::pci::{self, PciDevice};
use crate::memory::phys::GlobalFrameAllocator;
use crate::memory::virt;
use crate::sync::SpinLock;

const VIRTIO_VENDOR_ID: u16 = 0x1AF4;
const VIRTIO_BLK_DEVICE_ID: u16 = 0x1042;

/// Generic PCI vendor-specific capability ID (PCI Local Bus spec's own
/// reserved ID for vendor-defined capabilities) — not virtio-specific by
/// itself; every one of virtio-pci's five capability kinds uses it, with
/// `cfg_type` (read from inside the capability once found) the field
/// that actually distinguishes them.
const PCI_CAP_ID_VENDOR_SPECIFIC: u8 = 0x09;

// virtio-pci capability `cfg_type` values (virtio 1.0 spec §4.1.4).
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

// Device status bits (virtio 1.0 spec §2.1).
const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_DRIVER_OK: u8 = 4;

/// Feature bit 32 (`VIRTIO_F_VERSION_1`, virtio 1.0 spec §6) — the one
/// feature bit a modern-only (`-non-transitional`) device requires the
/// driver to accept, and the only one this driver negotiates: nothing
/// this milestone's read-only, single-request-at-a-time driver does
/// needs any of virtio-blk's own optional feature bits (`RO`,
/// `BLK_SIZE`, `FLUSH`, ...). Feature bits are read/written 32 at a
/// time via `*_feature_select`; bit 32 lives at `select = 1`, bit 0.
const VIRTIO_F_VERSION_1_SELECT: u32 = 1;
const VIRTIO_F_VERSION_1_BIT: u32 = 1;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;
const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;

const VIRTIO_BLK_T_IN: u32 = 0; // read
const VIRTIO_BLK_S_OK: u8 = 0;
/// Written into the status buffer before every request, and never a
/// real device-reported status code (those are `0`/`1`/`2` — virtio 1.0
/// spec §5.2.6) — lets a device that somehow completed without writing
/// a real status still be caught as `DeviceError` rather than
/// mistakenly read as success.
const STATUS_PENDING: u8 = 0xFF;

/// Fixed cap on sectors per [`VirtioBlk::read_sectors`] call — one 4 KiB
/// page's worth. This driver only ever has one request in flight at a
/// time (see this module's own doc comment), so this is a deliberate,
/// small, real limit matching what this milestone's own smoke tests and
/// Phase 5 fixture actually need, not a speculative one; revisit if a
/// real caller ever needs more in one call.
pub const MAX_SECTORS_PER_REQUEST: usize = 8;
const DATA_BUFFER_SIZE: usize = MAX_SECTORS_PER_REQUEST * SECTOR_SIZE;

/// Upper bound this driver clamps the device-reported max queue size to.
/// Exactly one request (a fixed 3-descriptor chain) is ever in flight,
/// so nothing here needs a large queue — `8` comfortably exceeds the
/// `3` minimum a single chain needs, with headroom to spare, without
/// requesting whatever much larger size (128 is typical) the device
/// happens to advertise.
const QUEUE_SIZE_CAP: u16 = 8;

// Common configuration structure offsets (virtio 1.0 spec §4.1.4.3).
const COMMON_DEVICE_FEATURE_SELECT: u64 = 0x00;
const COMMON_DEVICE_FEATURE: u64 = 0x04;
const COMMON_GUEST_FEATURE_SELECT: u64 = 0x08;
const COMMON_GUEST_FEATURE: u64 = 0x0C;
const COMMON_DEVICE_STATUS: u64 = 0x14;
const COMMON_QUEUE_SELECT: u64 = 0x16;
const COMMON_QUEUE_SIZE: u64 = 0x18;
const COMMON_QUEUE_ENABLE: u64 = 0x1C;
const COMMON_QUEUE_NOTIFY_OFF: u64 = 0x1E;
const COMMON_QUEUE_DESC: u64 = 0x20;
const COMMON_QUEUE_AVAIL: u64 = 0x28;
const COMMON_QUEUE_USED: u64 = 0x30;

/// Fixed virtual-address range this driver maps its device's PCI BARs
/// into, one fixed slot per BAR index — deliberately never through the
/// HHDM, the same `arch::x86_64::lapic::LAPIC_MMIO_VBASE` reasoning:
/// Limine's HHDM only promises to cover installed RAM, not a PCI BAR's
/// MMIO hole (confirmed the hard way for the LAPIC in `docs/adr/0009`).
/// Distinct from `LAPIC_MMIO_VBASE` (`0xffff_9500_...`) so the two never
/// collide. `BAR_VSTRIDE` (1 MiB) is comfortably larger than either of
/// this milestone's two real BARs (4 KiB and 16 KiB — Phase 1's own
/// findings), leaving headroom without needing a dynamic virtual-address
/// allocator for what is, structurally, always a small, fixed number of
/// BARs (at most 6, a type-0 PCI header's own limit).
const MMIO_VBASE: u64 = 0xffff_9600_0000_0000;
const BAR_VSTRIDE: u64 = 0x10_0000;

unsafe fn mmio_read8(addr: VirtAddr) -> u8 {
    unsafe { core::ptr::read_volatile(addr.as_ptr()) }
}
unsafe fn mmio_read16(addr: VirtAddr) -> u16 {
    unsafe { core::ptr::read_volatile(addr.as_ptr()) }
}
unsafe fn mmio_read32(addr: VirtAddr) -> u32 {
    unsafe { core::ptr::read_volatile(addr.as_ptr()) }
}
unsafe fn mmio_read64(addr: VirtAddr) -> u64 {
    unsafe { core::ptr::read_volatile(addr.as_ptr()) }
}
unsafe fn mmio_write8(addr: VirtAddr, value: u8) {
    unsafe { core::ptr::write_volatile(addr.as_mut_ptr(), value) }
}
unsafe fn mmio_write16(addr: VirtAddr, value: u16) {
    unsafe { core::ptr::write_volatile(addr.as_mut_ptr(), value) }
}
unsafe fn mmio_write32(addr: VirtAddr, value: u32) {
    unsafe { core::ptr::write_volatile(addr.as_mut_ptr(), value) }
}
unsafe fn mmio_write64(addr: VirtAddr, value: u64) {
    unsafe { core::ptr::write_volatile(addr.as_mut_ptr(), value) }
}

/// One virtio-pci capability this driver cares about, resolved down to
/// "which BAR, at what offset" — everything needed to compute a final
/// virtual address once that BAR is mapped.
#[derive(Clone, Copy)]
struct VirtioCap {
    bar: u8,
    offset: u32,
    #[allow(dead_code)]
    length: u32,
}

/// Allocates one fresh physical frame and returns both its physical
/// address (to hand to the device) and its HHDM-mapped virtual address
/// (for the CPU to read/write) — every frame this driver allocates is
/// ordinary usable RAM, which the HHDM always covers (unlike a PCI BAR's
/// MMIO hole — see [`map_bar`]), so no explicit mapping is needed here,
/// mirroring `memory::virt::AddressSpace::new`'s own frame-then-HHDM-
/// pointer pattern.
fn alloc_dma_frame() -> (PhysAddr, VirtAddr) {
    let frame = GlobalFrameAllocator
        .allocate_frame()
        .expect("out of physical memory setting up the virtio-blk driver");
    let phys = frame.start_address();
    let virt = virt::phys_to_virt(phys);
    // SAFETY: `virt` is this brand-new frame's own HHDM alias, exclusively
    // owned by this driver from this point on.
    unsafe {
        core::ptr::write_bytes(virt.as_mut_ptr::<u8>(), 0, 4096);
    }
    (phys, virt)
}

/// Maps `size_bytes` (rounded up to whole pages) of PCI BAR `bar_index`'s
/// physical MMIO region at the fixed virtual address [`MMIO_VBASE`]
/// reserves for that BAR index. Called at most once per distinct BAR
/// index actually referenced by one of this device's capabilities (see
/// `VirtioBlk::new`) — calling it twice for the same BAR would try to
/// map already-mapped pages and panic.
fn map_bar(device: PciDevice, bar_index: u8, size_bytes: u32) -> VirtAddr {
    let phys_base = PhysAddr::new(device.mmio_bar_address(bar_index));
    let virt_base = VirtAddr::new(MMIO_VBASE + (bar_index as u64) * BAR_VSTRIDE);
    let page_count = (size_bytes as u64).div_ceil(4096).max(1);
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_CACHE
        | PageTableFlags::NO_EXECUTE;
    for i in 0..page_count {
        let page = Page::<Size4KiB>::containing_address(virt_base + i * 4096);
        let frame = PhysFrame::<Size4KiB>::containing_address(phys_base + i * 4096);
        virt::map(page, frame, flags).expect("failed to map a virtio-blk BAR's MMIO page");
    }
    virt_base
}

pub struct VirtioBlk {
    notify_base: VirtAddr,
    notify_off_multiplier: u32,
    queue_notify_off: u16,
    capacity_sectors: u64,

    desc: VirtAddr,
    avail: VirtAddr,
    used: VirtAddr,
    queue_size: u16,

    header_phys: PhysAddr,
    header_virt: VirtAddr,
    data_phys: PhysAddr,
    data_virt: VirtAddr,
    status_phys: PhysAddr,
    status_virt: VirtAddr,
}

impl VirtioBlk {
    /// Discovers `device`'s virtio-pci capability list, maps its BARs,
    /// negotiates the minimal feature set, and sets up one virtqueue.
    /// `device` must already have MMIO decoding and bus mastering
    /// enabled (`PciDevice::enable_mmio_and_bus_master`).
    ///
    /// Panics (rather than returning an error) if `device` is missing a
    /// capability or queue capacity this driver requires: `device` was
    /// already confirmed, by [`init`]'s own PCI vendor/device ID match,
    /// to be the exact device this milestone's Phase 1 findings describe
    /// — a real one missing what the modern virtio-pci spec mandates
    /// means this driver's own understanding of it is wrong, which
    /// should fail loud during this milestone's own testing, not be
    /// silently tolerated as if it were merely "no disk attached today"
    /// (that case is [`init`]'s `find_device` returning `None`, handled
    /// separately, before this constructor ever runs).
    fn new(device: PciDevice) -> Self {
        let mut common: Option<VirtioCap> = None;
        let mut notify: Option<VirtioCap> = None;
        let mut notify_off_multiplier = 0u32;
        let mut device_cfg: Option<VirtioCap> = None;
        let mut bar_min_size = [0u32; 6];

        for (cap_id, cap_offset) in device.capabilities() {
            if cap_id != PCI_CAP_ID_VENDOR_SPECIFIC {
                continue;
            }
            let cfg_type = device.read_u8(cap_offset + 3);
            let bar = device.read_u8(cap_offset + 4);
            let offset = device.read_u32(cap_offset + 8);
            let length = device.read_u32(cap_offset + 12);
            let cap = VirtioCap {
                bar,
                offset,
                length,
            };
            bar_min_size[bar as usize] = bar_min_size[bar as usize].max(offset + length);
            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG => common = Some(cap),
                VIRTIO_PCI_CAP_NOTIFY_CFG => {
                    notify = Some(cap);
                    // The notify capability structure has one extra
                    // field, `notify_off_multiplier` (le32), right after
                    // the common 16-byte `virtio_pci_cap` header (virtio
                    // 1.0 spec §4.1.4.4).
                    notify_off_multiplier = device.read_u32(cap_offset + 16);
                }
                VIRTIO_PCI_CAP_DEVICE_CFG => device_cfg = Some(cap),
                _ => {}
            }
        }

        let common = common.expect("virtio-blk device missing its COMMON_CFG capability");
        let notify = notify.expect("virtio-blk device missing its NOTIFY_CFG capability");
        let device_cfg =
            device_cfg.expect("virtio-blk device missing its DEVICE_CFG capability");

        let mut bar_vbase: [Option<VirtAddr>; 6] = [None; 6];
        for (bar_index, &min_size) in bar_min_size.iter().enumerate() {
            if min_size > 0 {
                bar_vbase[bar_index] = Some(map_bar(device, bar_index as u8, min_size));
            }
        }
        let resolve = |cap: VirtioCap| -> VirtAddr {
            bar_vbase[cap.bar as usize].expect("BAR mapped above for every referenced index")
                + cap.offset as u64
        };

        let common_cfg = resolve(common);
        let notify_base = resolve(notify);
        let device_cfg_addr = resolve(device_cfg);

        // --- Feature negotiation (virtio 1.0 spec §3.1.1) ---
        unsafe {
            mmio_write8(common_cfg + COMMON_DEVICE_STATUS, 0);
            while mmio_read8(common_cfg + COMMON_DEVICE_STATUS) != 0 {
                core::hint::spin_loop();
            }
            mmio_write8(common_cfg + COMMON_DEVICE_STATUS, STATUS_ACKNOWLEDGE);
            mmio_write8(
                common_cfg + COMMON_DEVICE_STATUS,
                STATUS_ACKNOWLEDGE | STATUS_DRIVER,
            );

            mmio_write32(
                common_cfg + COMMON_DEVICE_FEATURE_SELECT,
                VIRTIO_F_VERSION_1_SELECT,
            );
            let high_features = mmio_read32(common_cfg + COMMON_DEVICE_FEATURE);
            assert!(
                high_features & VIRTIO_F_VERSION_1_BIT != 0,
                "virtio-blk device didn't offer VIRTIO_F_VERSION_1 -- expected for a \
                 `-non-transitional` device (see this milestone's Phase 1 findings)"
            );

            mmio_write32(common_cfg + COMMON_GUEST_FEATURE_SELECT, 0);
            mmio_write32(common_cfg + COMMON_GUEST_FEATURE, 0);
            mmio_write32(
                common_cfg + COMMON_GUEST_FEATURE_SELECT,
                VIRTIO_F_VERSION_1_SELECT,
            );
            mmio_write32(common_cfg + COMMON_GUEST_FEATURE, VIRTIO_F_VERSION_1_BIT);

            mmio_write8(
                common_cfg + COMMON_DEVICE_STATUS,
                STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK,
            );
            assert!(
                mmio_read8(common_cfg + COMMON_DEVICE_STATUS) & STATUS_FEATURES_OK != 0,
                "virtio-blk device rejected this driver's minimal feature set \
                 (VIRTIO_F_VERSION_1 only)"
            );
        }

        // --- Queue 0 setup ---
        let (desc_phys, desc_virt) = alloc_dma_frame();
        let (avail_phys, avail_virt) = alloc_dma_frame();
        let (used_phys, used_virt) = alloc_dma_frame();

        let (queue_size, queue_notify_off) = unsafe {
            mmio_write16(common_cfg + COMMON_QUEUE_SELECT, 0);
            let max_queue_size = mmio_read16(common_cfg + COMMON_QUEUE_SIZE);
            assert!(
                max_queue_size >= 3,
                "virtio-blk device's queue 0 max size ({max_queue_size}) is too small for \
                 this driver's fixed 3-descriptor request chain"
            );
            let queue_size = max_queue_size.min(QUEUE_SIZE_CAP);
            mmio_write16(common_cfg + COMMON_QUEUE_SIZE, queue_size);
            mmio_write64(common_cfg + COMMON_QUEUE_DESC, desc_phys.as_u64());
            mmio_write64(common_cfg + COMMON_QUEUE_AVAIL, avail_phys.as_u64());
            mmio_write64(common_cfg + COMMON_QUEUE_USED, used_phys.as_u64());
            let queue_notify_off = mmio_read16(common_cfg + COMMON_QUEUE_NOTIFY_OFF);
            mmio_write16(common_cfg + COMMON_QUEUE_ENABLE, 1);

            // No interrupt handler is ever registered for this device
            // (this milestone's own Non-goal — synchronous polling
            // only); telling the device not to bother raising one
            // avoids leaving its legacy INTx line asserted with nothing
            // ever reading ISR status to clear it.
            mmio_write16(avail_virt, VIRTQ_AVAIL_F_NO_INTERRUPT);

            mmio_write8(
                common_cfg + COMMON_DEVICE_STATUS,
                STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
            );

            (queue_size, queue_notify_off)
        };

        // A single read, at init, before any I/O has run and so before
        // anything could change the device's own config space -- no
        // `config_generation` torn-read guard needed for that reason
        // alone (see virtio 1.0 spec §4.1.4.3.1's own note that such
        // guards exist for structures that can change *during* a read).
        let capacity_sectors = unsafe { mmio_read64(device_cfg_addr) };

        let (header_phys, header_virt) = alloc_dma_frame();
        let (data_phys, data_virt) = alloc_dma_frame();
        let (status_phys, status_virt) = alloc_dma_frame();

        Self {
            notify_base,
            notify_off_multiplier,
            queue_notify_off,
            capacity_sectors,
            desc: desc_virt,
            avail: avail_virt,
            used: used_virt,
            queue_size,
            header_phys,
            header_virt,
            data_phys,
            data_virt,
            status_phys,
            status_virt,
        }
    }

    fn write_desc(&self, index: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let base = self.desc + (index as u64) * 16;
        unsafe {
            mmio_write64(base, addr);
            mmio_write32(base + 8, len);
            mmio_write16(base + 12, flags);
            mmio_write16(base + 14, next);
        }
    }

    fn read_avail_idx(&self) -> u16 {
        unsafe { mmio_read16(self.avail + 2) }
    }

    fn write_avail_idx(&self, value: u16) {
        unsafe { mmio_write16(self.avail + 2, value) }
    }

    fn write_avail_ring_entry(&self, slot: u16, desc_index: u16) {
        let addr = self.avail + 4 + (slot as u64) * 2;
        unsafe { mmio_write16(addr, desc_index) }
    }

    fn read_used_idx(&self) -> u16 {
        unsafe { mmio_read16(self.used + 2) }
    }

    fn notify_queue(&self) {
        let addr =
            self.notify_base + (self.queue_notify_off as u64) * (self.notify_off_multiplier as u64);
        unsafe { mmio_write16(addr, 0) }
    }
}

impl Driver for VirtioBlk {
    fn name(&self) -> &'static str {
        "virtio-blk"
    }
}

impl BlockDevice for VirtioBlk {
    fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }

    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if buf.is_empty() || !buf.len().is_multiple_of(SECTOR_SIZE) {
            return Err(BlockError::BufferNotSectorAligned);
        }
        let sector_count = (buf.len() / SECTOR_SIZE) as u64;
        if buf.len() > DATA_BUFFER_SIZE {
            return Err(BlockError::RequestTooLarge);
        }
        let end = lba
            .checked_add(sector_count)
            .ok_or(BlockError::OutOfRange)?;
        if end > self.capacity_sectors {
            return Err(BlockError::OutOfRange);
        }

        unsafe {
            mmio_write32(self.header_virt, VIRTIO_BLK_T_IN);
            mmio_write32(self.header_virt + 4, 0);
            mmio_write64(self.header_virt + 8, lba);
            mmio_write8(self.status_virt, STATUS_PENDING);
        }

        self.write_desc(0, self.header_phys.as_u64(), 16, VIRTQ_DESC_F_NEXT, 1);
        self.write_desc(
            1,
            self.data_phys.as_u64(),
            buf.len() as u32,
            VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
            2,
        );
        self.write_desc(2, self.status_phys.as_u64(), 1, VIRTQ_DESC_F_WRITE, 0);

        let avail_idx = self.read_avail_idx();
        self.write_avail_ring_entry(avail_idx % self.queue_size, 0);
        // Ensure the descriptor-chain and ring-entry writes above are
        // visible before the index update below publishes them -- the
        // device is a genuinely independent observer (DMA, no shared
        // cache-coherency protocol assumed beyond this fence).
        fence(Ordering::SeqCst);
        self.write_avail_idx(avail_idx.wrapping_add(1));
        fence(Ordering::SeqCst);
        self.notify_queue();

        let target = avail_idx.wrapping_add(1);
        while self.read_used_idx() != target {
            core::hint::spin_loop();
        }
        fence(Ordering::SeqCst);

        let status = unsafe { mmio_read8(self.status_virt) };
        if status != VIRTIO_BLK_S_OK {
            return Err(BlockError::DeviceError);
        }

        // SAFETY: `data_virt` is this driver's own DMA bounce buffer,
        // exclusively owned by it, and the device has just finished
        // writing exactly `buf.len()` bytes into it (confirmed by the
        // used-ring wait above).
        let data = unsafe { core::slice::from_raw_parts(self.data_virt.as_ptr::<u8>(), buf.len()) };
        buf.copy_from_slice(data);
        Ok(())
    }
}

static VIRTIO_BLK: SpinLock<Option<VirtioBlk>> = SpinLock::new(None);

/// Finds the virtio-blk device via PCI, maps its capabilities,
/// negotiates the minimal feature set, and sets up one virtqueue, for
/// [`with_device`] to use afterward. `Err` means no matching PCI device
/// exists — a normal build/run with no disk attached — never a panic;
/// see [`VirtioBlk::new`]'s own doc comment for what *does* panic and
/// why.
///
/// Logs its own outcome the same "prove it found what it claims to have
/// found" way `arch::x86_64::smp` logs AP bring-up — moved here from
/// `main.rs`'s own Phase 2/3 test-only boot block once this call became
/// unconditional (Phase 4), so every boot gets exactly one PCI scan
/// (`find_device`'s own cost), not this plus a second, separate
/// diagnostic-only one. Marker text (`PCI_ENUM_OK`/`PCI_ENUM_FAIL`)
/// unchanged from those earlier phases, so `xtask test-block-driver`'s
/// existing assertions still match.
pub fn init() -> Result<(), &'static str> {
    let device = match pci::find_device(VIRTIO_VENDOR_ID, VIRTIO_BLK_DEVICE_ID) {
        Some(device) => device,
        None => {
            crate::earlyprintln!("[pci-test] PCI_ENUM_FAIL -- virtio-blk device not found");
            return Err("no virtio-blk PCI device found");
        }
    };
    crate::earlyprintln!(
        "[pci-test] found virtio-blk at {:?} (vendor={:#06x} device={:#06x})",
        device.address,
        device.vendor_id,
        device.device_id
    );
    device.enable_mmio_and_bus_master();
    let bar1 = device.mmio_bar_address(1);
    let bar4 = device.mmio_bar_address(4);
    crate::earlyprintln!("[pci-test] BAR1={:#x} BAR4={:#x}", bar1, bar4);
    crate::earlyprintln!("[pci-test] PCI_ENUM_OK");
    *VIRTIO_BLK.lock() = Some(VirtioBlk::new(device));
    Ok(())
}

/// Runs `f` against the initialized driver, or `None` if [`init`] was
/// never called or found no device.
pub fn with_device<R>(f: impl FnOnce(&mut VirtioBlk) -> R) -> Option<R> {
    VIRTIO_BLK.lock().as_mut().map(f)
}
