//! Physical frame allocator.
//!
//! A bitmap allocator seeded from the Limine memory map. Simplicity over
//! throughput for this milestone — a buddy/slab allocator is a deliberate,
//! deferred performance follow-up (see `docs/adr`).
//!
//! The bitmap itself lives in a fixed-size BSS array rather than being
//! carved out of a scavenged physical region: it is part of the kernel
//! image, so it is already mapped by the time Limine hands off control,
//! with no chicken-and-egg problem against the allocator it backs.
//!
//! The actual bit-twiddling (`tarnos_kcore::Bitmap`) and the byte-range-
//! to-frame-range rounding (`tarnos_kcore::bitmap::usable_frame_range`)
//! live in `tarnos-kcore` instead of here, specifically so they're
//! unit-testable on the host — this module is a thin adapter that adds
//! back the hardware types (`PhysFrame`, the Limine `Entry`) a host-side
//! test has no use for.
use tarnos_kcore::bitmap::usable_frame_range;
use x86_64::structures::paging::{FrameAllocator, FrameDeallocator, PhysFrame, Size4KiB};
use x86_64::PhysAddr;

use limine::memmap::{Entry, MEMMAP_USABLE};

use crate::sync::SpinLock;

const FRAME_SIZE: u64 = 4096;
/// Upper bound on physical memory this allocator can track. Comfortably
/// beyond anything this milestone's QEMU targets use; a machine reporting
/// usable frames past this bound has those frames ignored (logged, not a
/// hard failure) rather than corrupting the bitmap.
const MAX_TRACKED_BYTES: u64 = 16 * 1024 * 1024 * 1024; // 16 GiB
const MAX_TRACKED_FRAMES: usize = (MAX_TRACKED_BYTES / FRAME_SIZE) as usize;
const BITMAP_WORDS: usize = MAX_TRACKED_FRAMES / 64;

type Bitmap = tarnos_kcore::Bitmap<BITMAP_WORDS>;

pub struct BitmapFrameAllocator {
    bitmap: Bitmap,
    /// Set once, at [`BitmapFrameAllocator::populate`] time, for every
    /// frame index this allocator was ever handed authority over (i.e.
    /// every `MEMMAP_USABLE` frame) -- and never modified again. Unlike
    /// `bitmap` (which flips a frame between free/allocated over its
    /// whole lifetime), this distinguishes "currently allocated" from
    /// "never ours to begin with" -- both read as `is_free == false` in
    /// `bitmap` alone, so `deallocate_frame`'s existing double-free check
    /// (`!bitmap.is_free`) can't tell a double-free of a real allocation
    /// apart from freeing, say, a kernel `.text` frame that was never in
    /// the usable pool at all. See `deallocate_frame`'s doc comment.
    usable: Bitmap,
}

impl BitmapFrameAllocator {
    const fn new() -> Self {
        Self {
            bitmap: Bitmap::new(),
            usable: Bitmap::new(),
        }
    }

    /// Marks every usable region from the Limine memory map as free.
    /// Must run exactly once, before any allocation.
    fn populate(&mut self, entries: &[&Entry]) {
        for entry in entries {
            if entry.type_ != MEMMAP_USABLE {
                continue;
            }
            for frame_index in usable_frame_range(entry.base, entry.length, FRAME_SIZE) {
                self.bitmap.set_free(frame_index);
                self.usable.set_free(frame_index);
            }
        }
    }

    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let frame_index = self.bitmap.allocate()?;
        let addr = PhysAddr::new(frame_index as u64 * FRAME_SIZE);
        Some(PhysFrame::from_start_address(addr).expect("frame index is always frame-aligned"))
    }

    /// # Safety
    /// `frame` must currently be allocated and not referenced by any live
    /// mapping or in-flight DMA.
    ///
    /// Checks two distinct misuse cases, both fatal in debug builds: a
    /// genuine double-free (already back in the free list), and freeing
    /// a frame this allocator never owned in the first place (outside
    /// every `MEMMAP_USABLE` region -- kernel `.text`/reserved/MMIO,
    /// or a garbage address from a corrupted page-table walk). The
    /// second case matters on its own: a stray `deallocate_frame` call
    /// with a bogus physical address would otherwise silently mark that
    /// frame "free" for the *first* time (its `bitmap` bit reads
    /// `is_free == false`, identical to a legitimately-owned allocated
    /// frame, so the double-free check alone can't catch it) -- then
    /// hand it back out to something else via a later `allocate_frame`,
    /// corrupting whatever physical memory that frame actually holds
    /// (observed in practice: a process's `AddressSpace` teardown
    /// freeing a frame that turned out to still be part of the kernel's
    /// own mapped `.text`, which the next allocation then overwrote --
    /// see `docs/adr/0011`).
    unsafe fn deallocate_frame(&mut self, frame: PhysFrame<Size4KiB>) {
        let frame_index = (frame.start_address().as_u64() / FRAME_SIZE) as usize;
        debug_assert!(
            self.usable.is_free(frame_index),
            "freeing physical frame {:#x}, which was never part of the usable pool \
             (kernel .text/reserved/MMIO, or a corrupted page-table entry?)",
            frame.start_address()
        );
        debug_assert!(
            !self.bitmap.is_free(frame_index),
            "double free of physical frame {:#x}",
            frame.start_address()
        );
        self.bitmap.set_free(frame_index);
    }
}

// `SpinLock`, not a plain `spin::Mutex`: nothing in this kernel calls into
// the frame allocator from interrupt context today, but leaving that
// undocumented and relying on it implicitly is exactly the kind of gap
// this milestone's own hardening sweep exists to close -- unlike every
// other lock in the codebase, this one had no stated rationale either
// way. Costs nothing (this lock is never contended from an interrupt
// handler, so the extra interrupt-disable is unobservable), and makes
// the safety property explicit and enforced rather than assumed.
static ALLOCATOR: SpinLock<BitmapFrameAllocator> = SpinLock::new(BitmapFrameAllocator::new());

/// Seeds the global frame allocator from Limine's memory map. Must be
/// called exactly once, before any other `memory::phys` function.
pub fn init(entries: &[&Entry]) {
    ALLOCATOR.lock().populate(entries);
}

/// Number of physical frames currently free. Exists for leak-regression
/// testing (`xtask test-process-lifecycle`): a sequence of process
/// creation/destruction that doesn't actually leak memory should leave
/// this exactly where it started.
pub fn free_frame_count() -> usize {
    ALLOCATOR.lock().bitmap.free_count()
}

/// A thin handle implementing the `x86_64` crate's [`FrameAllocator`] /
/// [`FrameDeallocator`] traits by delegating to the global bitmap
/// allocator. Zero-sized — construct one wherever those traits are
/// required (e.g. as an argument to `Mapper::map_to`).
pub struct GlobalFrameAllocator;

unsafe impl FrameAllocator<Size4KiB> for GlobalFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        ALLOCATOR.lock().allocate_frame()
    }
}

impl FrameDeallocator<Size4KiB> for GlobalFrameAllocator {
    unsafe fn deallocate_frame(&mut self, frame: PhysFrame<Size4KiB>) {
        unsafe {
            ALLOCATOR.lock().deallocate_frame(frame);
        }
    }
}
