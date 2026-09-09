//! Virtual memory: the single place that touches raw page table memory.
//!
//! Everything above this module (the ELF loader, process creation, the
//! heap) works through the safe, typed [`map`]/[`unmap`]/[`translate`]
//! functions here — never by walking or dereferencing a page table entry
//! directly. That boundary is what "zero-trust memory" means in practice:
//! the unsafety of raw paging is audited once, here, instead of trusted
//! ad hoc at every call site that needs a mapping.
use spin::{Mutex, Once};
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{
    Mapper as X86Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
    Translate,
};
use x86_64::{PhysAddr, VirtAddr};

use super::phys::GlobalFrameAllocator;

static HHDM_OFFSET: Once<VirtAddr> = Once::new();
static MAPPER: Once<Mutex<OffsetPageTable<'static>>> = Once::new();

/// Converts a physical address to the kernel-accessible virtual address at
/// which it appears via Limine's Higher Half Direct Map. Valid only after
/// [`init`] has run.
pub fn phys_to_virt(phys: PhysAddr) -> VirtAddr {
    let offset = *HHDM_OFFSET
        .get()
        .expect("memory::virt::init() must run before phys_to_virt()");
    offset + phys.as_u64()
}

/// # Safety
/// `hhdm_offset` must be the offset Limine actually reports via its HHDM
/// request, and the currently-loaded CR3 must point at a valid, complete
/// page table hierarchy (true immediately after Limine hands off control,
/// before anything has touched CR3).
pub unsafe fn init(hhdm_offset: VirtAddr) {
    HHDM_OFFSET.call_once(|| hhdm_offset);

    let (level_4_frame, _) = Cr3::read();
    let level_4_table_ptr: *mut PageTable =
        phys_to_virt(level_4_frame.start_address()).as_mut_ptr();

    // SAFETY: forwarding the caller's guarantee that CR3 and hhdm_offset
    // are both valid — this is the one place in the kernel allowed to
    // conjure a `&mut PageTable` from a raw address.
    let level_4_table: &'static mut PageTable = unsafe { &mut *level_4_table_ptr };
    let mapper = unsafe { OffsetPageTable::new(level_4_table, hhdm_offset) };
    MAPPER.call_once(|| Mutex::new(mapper));
}

fn with_mapper<R>(f: impl FnOnce(&mut OffsetPageTable<'static>) -> R) -> R {
    let mapper = MAPPER
        .get()
        .expect("memory::virt::init() must run before map()/unmap()/translate()");
    f(&mut mapper.lock())
}

#[derive(Debug)]
pub enum MapError {
    /// The frame allocator ran out of physical memory (for the mapping
    /// itself or for an intermediate page table it needed to create).
    OutOfMemory,
    /// `page` is already mapped to some frame.
    AlreadyMapped,
}

/// Maps a single 4 KiB page to a physical frame with the given flags.
///
/// This is the only function in the kernel that calls the `x86_64` crate's
/// unsafe `Mapper::map_to` — every caller goes through this typed,
/// bounds-checked-by-construction wrapper instead.
pub fn map(page: Page<Size4KiB>, frame: PhysFrame<Size4KiB>, flags: PageTableFlags) -> Result<(), MapError> {
    with_mapper(|mapper| {
        let mut allocator = GlobalFrameAllocator;
        // SAFETY: the caller supplies a `frame` obtained from the global
        // frame allocator (via GlobalFrameAllocator elsewhere), which
        // guarantees it is not aliased by any other live mapping.
        unsafe { mapper.map_to(page, frame, flags, &mut allocator) }
            .map(|flush| flush.flush())
            .map_err(|e| match e {
                x86_64::structures::paging::mapper::MapToError::FrameAllocationFailed => {
                    MapError::OutOfMemory
                }
                x86_64::structures::paging::mapper::MapToError::PageAlreadyMapped(_) => {
                    MapError::AlreadyMapped
                }
                x86_64::structures::paging::mapper::MapToError::ParentEntryHugePage => {
                    MapError::AlreadyMapped
                }
            })
    })
}

/// Removes the mapping for `page`, returning the frame it was mapped to.
/// Does not free the frame — callers that own the frame decide whether to
/// return it to the allocator.
pub fn unmap(page: Page<Size4KiB>) -> Option<PhysFrame<Size4KiB>> {
    with_mapper(|mapper| mapper.unmap(page).ok().map(|(frame, flush)| {
        flush.flush();
        frame
    }))
}

/// Looks up the physical address a virtual address currently maps to, or
/// `None` if it is unmapped.
pub fn translate(addr: VirtAddr) -> Option<PhysAddr> {
    with_mapper(|mapper| mapper.translate_addr(addr))
}
