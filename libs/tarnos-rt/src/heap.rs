//! Userland global allocator: backs `alloc::*` (`Box`, `Vec`, `String`,
//! ...) for any TarnOS-native binary that links `tarnos-rt`.
//!
//! Unlike the kernel's own heap (`kernel/src/memory/heap.rs`, which
//! reserves and maps a fixed region eagerly at boot), this one starts
//! genuinely empty and grows lazily on demand via `sys_sbrk` — userland
//! has no boot-time knowledge of how much memory it'll ever need, so
//! there is nothing sensible to reserve up front.
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

use linked_list_allocator::LockedHeap;

use crate::syscall;

/// Minimum amount to grow by on each `sys_sbrk` call, even for a small
/// allocation — amortizes the syscall's cost the same way a real libc
/// allocator amortizes `brk`/`mmap` calls, instead of trapping into the
/// kernel on every single `alloc`.
const MIN_GROW_BYTES: usize = 16 * 1024;

struct UserHeap {
    inner: LockedHeap,
    /// `linked_list_allocator::Heap::extend` panics if called before the
    /// heap has ever been `init`-ed — `LockedHeap::empty()` starts with
    /// no backing memory at all, so the very first growth must go
    /// through `init`, and only growths after that through `extend`.
    initialized: AtomicBool,
}

impl UserHeap {
    const fn new() -> Self {
        Self {
            inner: LockedHeap::empty(),
            initialized: AtomicBool::new(false),
        }
    }

    /// Grows the heap by at least `min_bytes` via `sys_sbrk`, then hands
    /// the newly-mapped memory to the inner allocator. Returns `false`
    /// if `sys_sbrk` itself failed (heap ceiling reached, or the kernel
    /// is out of physical memory) — callers turn that into a null
    /// allocation rather than panicking, same as any other allocator
    /// failure.
    fn grow(&self, min_bytes: usize) -> bool {
        let Ok(old_end) = syscall::sys_sbrk(min_bytes as i64) else {
            return false;
        };
        // SAFETY: `sys_sbrk` just mapped exactly `min_bytes` of fresh,
        // process-owned memory directly following `old_end` — which is
        // either the heap's original bottom (first call) or the address
        // immediately after everything handed to the allocator so far
        // (every later call), matching what `init`/`extend` each
        // require.
        unsafe {
            if self.initialized.swap(true, Ordering::Relaxed) {
                self.inner.lock().extend(min_bytes);
            } else {
                self.inner.lock().init(old_end as *mut u8, min_bytes);
            }
        }
        true
    }
}

unsafe impl GlobalAlloc for UserHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if let Ok(ptr) = self.inner.lock().allocate_first_fit(layout) {
            return ptr.as_ptr();
        }
        let grow_by = layout.size().max(MIN_GROW_BYTES);
        if !self.grow(grow_by) {
            return core::ptr::null_mut();
        }
        self.inner
            .lock()
            .allocate_first_fit(layout)
            .map(|ptr| ptr.as_ptr())
            .unwrap_or(core::ptr::null_mut())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(ptr) = NonNull::new(ptr) {
            // SAFETY: forwarding the caller's guarantee that `ptr`/`layout`
            // match a prior `alloc` call, as `GlobalAlloc::dealloc` requires.
            unsafe { self.inner.lock().deallocate(ptr, layout) };
        }
    }
}

#[global_allocator]
static ALLOCATOR: UserHeap = UserHeap::new();
