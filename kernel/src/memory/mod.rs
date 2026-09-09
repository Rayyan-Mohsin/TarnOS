//! Memory management.
//!
//! Three layers, each with one clear job:
//! - [`phys`]: which physical frames are free.
//! - [`virt`]: the sole boundary that maps/unmaps/translates virtual
//!   addresses — the only module allowed to touch raw page table memory.
//! - [`heap`]: backs `alloc::*` for the rest of the kernel.
//!
//! Re-exports the address/frame/page types from the audited `x86_64`
//! crate rather than redefining them, so the rest of the kernel imports
//! them from `crate::memory` without reaching into `x86_64` directly.
pub mod heap;
pub mod phys;
pub mod virt;

pub use x86_64::structures::paging::{Page, PageTableFlags, PhysFrame, Size4KiB};
pub use x86_64::{PhysAddr, VirtAddr};

use limine::memmap::Entry;

/// Brings up physical frame tracking, the kernel's own page table mapper,
/// and the kernel heap, in that order (each depends on the previous).
///
/// # Safety
/// `hhdm_offset` must be the value Limine's HHDM request actually
/// returned, and this must run before anything else touches CR3 or
/// allocates memory.
pub unsafe fn init(hhdm_offset: VirtAddr, memmap: &[&Entry]) {
    phys::init(memmap);
    unsafe {
        virt::init(hhdm_offset);
    }
    heap::init();
}
