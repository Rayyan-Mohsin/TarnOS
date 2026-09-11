//! Virtual memory: the single place that touches raw page table memory.
//!
//! Everything above this module (the ELF loader, process creation, the
//! heap) works through the safe, typed [`map`]/[`unmap`]/[`translate`]
//! functions here — never by walking or dereferencing a page table entry
//! directly. That boundary is what "zero-trust memory" means in practice:
//! the unsafety of raw paging is audited once, here, instead of trusted
//! ad hoc at every call site that needs a mapping.
use spin::{Mutex, Once};
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::{
    FrameAllocator, FrameDeallocator, Mapper as X86Mapper, OffsetPageTable, Page, PageTable,
    PageTableFlags, PhysFrame, Size4KiB, Translate,
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
            .map_err(map_error_from)
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

fn map_error_from(e: x86_64::structures::paging::mapper::MapToError<Size4KiB>) -> MapError {
    use x86_64::structures::paging::mapper::MapToError;
    match e {
        MapToError::FrameAllocationFailed => MapError::OutOfMemory,
        MapToError::PageAlreadyMapped(_) | MapToError::ParentEntryHugePage => {
            MapError::AlreadyMapped
        }
    }
}

/// A process's own address space: a PML4 distinct from the kernel's
/// boot-time one, with the kernel half (the canonical upper half, PML4
/// indices 256..512) copied from it so every process has the kernel
/// mapped identically — required for interrupt/syscall entry to work no
/// matter which process's page tables are active when a trap occurs.
pub struct AddressSpace {
    pml4_frame: PhysFrame<Size4KiB>,
}

impl AddressSpace {
    pub fn new() -> Result<Self, MapError> {
        let mut allocator = GlobalFrameAllocator;
        let frame = allocator.allocate_frame().ok_or(MapError::OutOfMemory)?;
        let new_table: &mut PageTable =
            unsafe { &mut *phys_to_virt(frame.start_address()).as_mut_ptr() };
        new_table.zero();
        with_mapper(|mapper| {
            let master = mapper.level_4_table();
            for i in 256..512 {
                new_table[i] = master[i].clone();
            }
        });
        Ok(Self { pml4_frame: frame })
    }

    pub fn pml4_frame(&self) -> PhysFrame<Size4KiB> {
        self.pml4_frame
    }

    /// Builds a fresh `OffsetPageTable` bound to this address space's
    /// PML4, independent of the currently-active one — this is what lets
    /// process creation (and the ELF loader, called through it) populate
    /// a process's memory before that process ever runs and its address
    /// space becomes the active one.
    ///
    /// # Safety
    /// `memory::virt::init` must already have run. The caller must not
    /// hold another live `OffsetPageTable` for this same `AddressSpace`
    /// at the same time (mutable-aliasing hazard) — in practice, use one
    /// at a time and let it drop before calling this again.
    pub unsafe fn mapper(&self) -> OffsetPageTable<'static> {
        let hhdm_offset = *HHDM_OFFSET
            .get()
            .expect("memory::virt::init() must run before AddressSpace::mapper()");
        let table: &'static mut PageTable =
            unsafe { &mut *phys_to_virt(self.pml4_frame.start_address()).as_mut_ptr() };
        unsafe { OffsetPageTable::new(table, hhdm_offset) }
    }

    /// Convenience wrapper around [`AddressSpace::mapper`] for mapping a
    /// single page (a stack page, or a code page remapped for the
    /// dummy-process scheduler smoke test) without the caller needing to
    /// juggle an `OffsetPageTable` itself.
    pub fn map(
        &self,
        page: Page<Size4KiB>,
        frame: PhysFrame<Size4KiB>,
        flags: PageTableFlags,
    ) -> Result<(), MapError> {
        let mut mapper = unsafe { self.mapper() };
        let mut allocator = GlobalFrameAllocator;
        unsafe { mapper.map_to(page, frame, flags, &mut allocator) }
            .map(|flush| flush.flush())
            .map_err(map_error_from)
    }

    /// Switches CR3 to this address space.
    ///
    /// # Safety
    /// Every address this address space's kernel half maps must match
    /// the currently-executing code's expectations (true for any
    /// `AddressSpace` built by [`AddressSpace::new`]), and the caller
    /// must be prepared for every subsequent memory access to go through
    /// these page tables instead of whichever were active before.
    pub unsafe fn activate(&self) {
        unsafe {
            Cr3::write(self.pml4_frame, Cr3Flags::empty());
        }
    }
}

/// Recursively frees every frame in the page-table subtree rooted at
/// `frame` — including `frame` itself — down through leaf mappings.
/// `level` is `frame`'s own page-table level (3=PDPT, 2=PD, 1=PT): a
/// level-1 table (a PT)'s entries are leaf page mappings, not further
/// tables, so they're freed directly rather than recursed into as if
/// they were another table's frame — recursing into a leaf data page as
/// though it were a `PageTable` would walk whatever that page's actual
/// contents happen to be, corrupting or crashing on unrelated memory.
///
/// Only ever called on frames that belong to an `AddressSpace` being
/// torn down in its entirety (see `Drop for AddressSpace` below) — by
/// that point nothing else can still be relying on this subtree.
fn free_table_subtree(frame: PhysFrame<Size4KiB>, level: u8) {
    // SAFETY: `frame` is a live page-table frame (guaranteed by the
    // caller — see this function's doc comment), so its HHDM alias is a
    // valid `PageTable`.
    let table: &PageTable = unsafe { &*phys_to_virt(frame.start_address()).as_ptr() };
    let mut allocator = GlobalFrameAllocator;
    for entry in table.iter() {
        // `entry.frame()` is `Err` for a not-present entry, and
        // (defensively — this kernel never creates one) for a huge
        // page, which has no child table to recurse into.
        if let Ok(child) = entry.frame() {
            if level > 1 {
                free_table_subtree(child, level - 1);
            } else {
                // SAFETY: `child` is a leaf page this address space is
                // being torn down in its entirety, so nothing else
                // still references it — same as `frame` below.
                unsafe { allocator.deallocate_frame(child) };
            }
        }
    }
    // SAFETY: same as above — `frame` is being freed as part of tearing
    // down the whole address space it belongs to, so nothing else still
    // references it.
    unsafe { allocator.deallocate_frame(frame) };
}

/// Frees every physical frame this address space owns: every mapped
/// user page (ELF segments, the user stack), every intermediate
/// PDPT/PD/PT frame `AddressSpace::map`'s calls to `map_to` allocated
/// along the way (never tracked anywhere else — this walk is the only
/// way to find them again), and the PML4 frame itself.
///
/// Walks only PML4 indices `0..256` — the user half. Indices `256..512`
/// (the kernel half every `AddressSpace` shares, copied by value at
/// construction — see `AddressSpace::new`) are never touched or
/// recursed into, so the heap, every process's kernel stack, and the
/// double-fault stack (all reachable only through that shared upper
/// half, per `docs/adr/0005`) are structurally unreachable from this
/// walk and can never be freed by it.
impl Drop for AddressSpace {
    fn drop(&mut self) {
        // SAFETY: this is this `AddressSpace`'s own PML4 frame, valid
        // for as long as `self` is (i.e. right up until this drop).
        let table: &PageTable =
            unsafe { &*phys_to_virt(self.pml4_frame.start_address()).as_ptr() };
        for entry in table.iter().take(256) {
            if let Ok(child) = entry.frame() {
                free_table_subtree(child, 3);
            }
        }
        let mut allocator = GlobalFrameAllocator;
        // SAFETY: every reference into the subtrees above is gone (they
        // were just freed), and this is the last use of `pml4_frame`
        // before `self` itself goes away.
        unsafe { allocator.deallocate_frame(self.pml4_frame) };
    }
}
