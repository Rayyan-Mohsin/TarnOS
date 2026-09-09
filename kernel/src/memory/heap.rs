//! Kernel heap: backs `alloc::*` (`Box`, `Vec`, `Arc`, ...) in this
//! `#![no_std]` binary.
use linked_list_allocator::LockedHeap;
use x86_64::structures::paging::{Page, PageTableFlags, Size4KiB};
use x86_64::VirtAddr;

use super::phys::GlobalFrameAllocator;
use super::virt;
use x86_64::structures::paging::FrameAllocator;

/// Fixed virtual base for the kernel heap: chosen to sit well clear of
/// both the HHDM region (which only needs to span installed physical RAM,
/// nowhere near this address for any machine this milestone targets) and
/// the kernel image itself (linked at `0xffffffff80000000`).
const HEAP_START: u64 = 0xffff_9000_0000_0000;
const HEAP_SIZE: u64 = 8 * 1024 * 1024; // 8 MiB to start; grows in a later milestone.

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// Maps and initializes the kernel heap. Must run after `memory::virt`
/// (page tables) and `memory::phys` (frame allocator) are both
/// initialized, and exactly once.
pub fn init() {
    let start_page = Page::<Size4KiB>::containing_address(VirtAddr::new(HEAP_START));
    let end_page = Page::<Size4KiB>::containing_address(VirtAddr::new(HEAP_START + HEAP_SIZE - 1));

    let mut allocator = GlobalFrameAllocator;
    for page in Page::range_inclusive(start_page, end_page) {
        let frame = allocator
            .allocate_frame()
            .expect("out of physical memory while mapping the kernel heap");
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
        virt::map(page, frame, flags).expect("failed to map kernel heap page");
    }

    unsafe {
        ALLOCATOR
            .lock()
            .init(HEAP_START as *mut u8, HEAP_SIZE as usize);
    }
}
