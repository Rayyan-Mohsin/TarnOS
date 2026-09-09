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
use spin::Mutex;
use x86_64::structures::paging::{FrameAllocator, FrameDeallocator, PhysFrame, Size4KiB};
use x86_64::PhysAddr;

use limine::memmap::{Entry, MEMMAP_USABLE};

const FRAME_SIZE: u64 = 4096;
/// Upper bound on physical memory this allocator can track. Comfortably
/// beyond anything this milestone's QEMU targets use; a machine reporting
/// usable frames past this bound has those frames ignored (logged, not a
/// hard failure) rather than corrupting the bitmap.
const MAX_TRACKED_BYTES: u64 = 16 * 1024 * 1024 * 1024; // 16 GiB
const MAX_TRACKED_FRAMES: usize = (MAX_TRACKED_BYTES / FRAME_SIZE) as usize;
const BITMAP_WORDS: usize = MAX_TRACKED_FRAMES / 64;

struct Bitmap {
    words: [u64; BITMAP_WORDS],
    /// One past the highest frame index ever marked usable; bounds search
    /// so allocation doesn't scan tens of thousands of always-reserved
    /// words past the top of installed RAM.
    frame_count: usize,
}

impl Bitmap {
    const fn new() -> Self {
        Self {
            words: [0; BITMAP_WORDS],
            frame_count: 0,
        }
    }

    fn set_free(&mut self, frame_index: usize) {
        if frame_index >= MAX_TRACKED_FRAMES {
            return;
        }
        self.words[frame_index / 64] |= 1 << (frame_index % 64);
        if frame_index >= self.frame_count {
            self.frame_count = frame_index + 1;
        }
    }

    fn set_used(&mut self, frame_index: usize) {
        if frame_index >= MAX_TRACKED_FRAMES {
            return;
        }
        self.words[frame_index / 64] &= !(1 << (frame_index % 64));
    }

    fn is_free(&self, frame_index: usize) -> bool {
        frame_index < MAX_TRACKED_FRAMES
            && (self.words[frame_index / 64] & (1 << (frame_index % 64))) != 0
    }

    fn allocate(&mut self) -> Option<usize> {
        for word_index in 0..(self.frame_count.div_ceil(64)) {
            let word = self.words[word_index];
            if word != 0 {
                let bit = word.trailing_zeros() as usize;
                let frame_index = word_index * 64 + bit;
                self.set_used(frame_index);
                return Some(frame_index);
            }
        }
        None
    }
}

pub struct BitmapFrameAllocator {
    bitmap: Bitmap,
}

impl BitmapFrameAllocator {
    const fn new() -> Self {
        Self {
            bitmap: Bitmap::new(),
        }
    }

    /// Marks every usable region from the Limine memory map as free.
    /// Must run exactly once, before any allocation.
    fn populate(&mut self, entries: &[&Entry]) {
        for entry in entries {
            if entry.type_ != MEMMAP_USABLE {
                continue;
            }
            let start_frame = entry.base.div_ceil(FRAME_SIZE);
            let end_frame = (entry.base + entry.length) / FRAME_SIZE;
            for frame_index in start_frame..end_frame {
                self.bitmap.set_free(frame_index as usize);
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
    unsafe fn deallocate_frame(&mut self, frame: PhysFrame<Size4KiB>) {
        let frame_index = (frame.start_address().as_u64() / FRAME_SIZE) as usize;
        debug_assert!(
            !self.bitmap.is_free(frame_index),
            "double free of physical frame {:#x}",
            frame.start_address()
        );
        self.bitmap.set_free(frame_index);
    }
}

static ALLOCATOR: Mutex<BitmapFrameAllocator> = Mutex::new(BitmapFrameAllocator::new());

/// Seeds the global frame allocator from Limine's memory map. Must be
/// called exactly once, before any other `memory::phys` function.
pub fn init(entries: &[&Entry]) {
    ALLOCATOR.lock().populate(entries);
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
