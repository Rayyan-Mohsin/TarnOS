//! A fixed-size, allocation-free bitmap set, plus the byte-range-to-
//! frame-index-range rounding logic a physical frame allocator needs
//! when consuming a firmware-supplied memory map.
//!
//! Split out of `tarnos-kernel`'s `memory::phys` so both pieces are
//! host-testable without pulling in `PhysFrame`/`x86_64` or a Limine
//! memory-map type — the kernel's `BitmapFrameAllocator` is a thin
//! adapter over [`Bitmap`] that adds those hardware types back.
use core::ops::Range;

/// A set of `WORDS * 64` indices, each either "free" (bit set) or "used"
/// (bit clear) — deliberately the inverse of the more common "1 = used"
/// convention, chosen so a freshly zeroed `Bitmap` (its `const fn new`)
/// starts with every index used/unavailable until explicitly marked
/// free by whoever seeds it, rather than accidentally treating untouched
/// memory as allocatable.
pub struct Bitmap<const WORDS: usize> {
    words: [u64; WORDS],
    /// One past the highest index ever marked free — bounds `allocate`'s
    /// search so it doesn't scan words that were never populated.
    frame_count: usize,
}

impl<const WORDS: usize> Bitmap<WORDS> {
    pub const CAPACITY: usize = WORDS * 64;

    pub const fn new() -> Self {
        Self {
            words: [0; WORDS],
            frame_count: 0,
        }
    }

    /// Marks `index` free (allocatable). Silently ignores an
    /// out-of-range index rather than panicking — a caller seeding this
    /// bitmap from a firmware memory map larger than `CAPACITY` should
    /// have those extra frames dropped, not crash the kernel over
    /// memory it wasn't going to be able to track anyway.
    pub fn set_free(&mut self, index: usize) {
        if index >= Self::CAPACITY {
            return;
        }
        self.words[index / 64] |= 1 << (index % 64);
        if index >= self.frame_count {
            self.frame_count = index + 1;
        }
    }

    /// Marks `index` used (not allocatable).
    pub fn set_used(&mut self, index: usize) {
        if index >= Self::CAPACITY {
            return;
        }
        self.words[index / 64] &= !(1 << (index % 64));
    }

    pub fn is_free(&self, index: usize) -> bool {
        index < Self::CAPACITY && (self.words[index / 64] & (1 << (index % 64))) != 0
    }

    /// Number of indices currently marked free. Independent of
    /// `CAPACITY` (which includes indices never seeded at all) — this is
    /// "how much is actually available right now," the figure a
    /// leak-regression test compares before and after a sequence of
    /// allocate/free cycles that should leave it unchanged.
    pub fn free_count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Finds and claims the lowest-indexed free slot, or `None` if
    /// nothing is free.
    pub fn allocate(&mut self) -> Option<usize> {
        for word_index in 0..self.frame_count.div_ceil(64) {
            let word = self.words[word_index];
            if word != 0 {
                let bit = word.trailing_zeros() as usize;
                let index = word_index * 64 + bit;
                self.set_used(index);
                return Some(index);
            }
        }
        None
    }
}

impl<const WORDS: usize> Default for Bitmap<WORDS> {
    fn default() -> Self {
        Self::new()
    }
}

/// Converts a byte range `[base, base + length)` into the range of
/// whole-frame indices it fully covers, rounding the start *up* and the
/// end *down* — a partial frame at either edge of a memory region is
/// never treated as usable, since handing out a frame that's only
/// partially inside a firmware-reported usable region would let the
/// allocator claim memory the firmware never actually promised was free.
///
/// Returns an empty range (never panics) if the input is shorter than
/// one frame or otherwise rounds away to nothing.
pub fn usable_frame_range(base: u64, length: u64, frame_size: u64) -> Range<usize> {
    let start_frame = base.div_ceil(frame_size);
    let end_frame = (base + length) / frame_size;
    (start_frame as usize)..(end_frame as usize)
}

#[cfg(test)]
mod tests {
    use super::{usable_frame_range, Bitmap};

    #[test]
    fn allocate_on_empty_bitmap_returns_none() {
        let mut b: Bitmap<4> = Bitmap::new();
        assert_eq!(b.allocate(), None);
    }

    #[test]
    fn allocate_then_free_then_allocate_returns_same_index() {
        let mut b: Bitmap<4> = Bitmap::new();
        b.set_free(10);
        assert!(b.is_free(10));
        assert_eq!(b.allocate(), Some(10));
        assert!(!b.is_free(10));
        b.set_free(10);
        assert!(b.is_free(10));
        assert_eq!(b.allocate(), Some(10));
    }

    #[test]
    fn allocate_picks_lowest_free_index() {
        let mut b: Bitmap<4> = Bitmap::new();
        b.set_free(5);
        b.set_free(2);
        b.set_free(9);
        assert_eq!(b.allocate(), Some(2));
        assert_eq!(b.allocate(), Some(5));
        assert_eq!(b.allocate(), Some(9));
        assert_eq!(b.allocate(), None);
    }

    #[test]
    fn allocate_until_exhausted_yields_every_free_index_exactly_once() {
        let mut b: Bitmap<2> = Bitmap::new();
        for i in 0..Bitmap::<2>::CAPACITY {
            b.set_free(i);
        }
        let mut seen = alloc_free_indices(&mut b);
        seen.sort_unstable();
        let expected: Vec<usize> = (0..Bitmap::<2>::CAPACITY).collect();
        assert_eq!(seen, expected);
        assert_eq!(b.allocate(), None);
    }

    fn alloc_free_indices<const W: usize>(b: &mut Bitmap<W>) -> Vec<usize> {
        let mut out = Vec::new();
        while let Some(i) = b.allocate() {
            out.push(i);
        }
        out
    }

    #[test]
    fn out_of_range_index_is_ignored_not_panicking() {
        let mut b: Bitmap<1> = Bitmap::new(); // capacity = 64
        b.set_free(1000); // silently dropped
        assert!(!b.is_free(1000));
        assert_eq!(b.allocate(), None);
    }

    #[test]
    fn boundary_index_at_top_of_capacity_works() {
        let mut b: Bitmap<1> = Bitmap::new(); // capacity = 64, valid indices 0..64
        let last = Bitmap::<1>::CAPACITY - 1;
        b.set_free(last);
        assert!(b.is_free(last));
        assert_eq!(b.allocate(), Some(last));
        // One past the top is out of range and must not alias index 0.
        b.set_free(Bitmap::<1>::CAPACITY);
        assert!(!b.is_free(0));
    }

    #[test]
    fn free_count_tracks_allocate_and_set_free() {
        let mut b: Bitmap<2> = Bitmap::new();
        assert_eq!(b.free_count(), 0);
        b.set_free(0);
        b.set_free(5);
        b.set_free(9);
        assert_eq!(b.free_count(), 3);
        b.allocate();
        assert_eq!(b.free_count(), 2);
        b.set_free(0);
        assert_eq!(b.free_count(), 3);
    }

    #[test]
    fn frame_range_rounds_up_start_and_down_end() {
        // A region starting 1 byte into frame 1 and running to 1 byte
        // into frame 3 only fully covers frame 2.
        let r = usable_frame_range(4097, 8192, 4096);
        assert_eq!(r, 2..3);
    }

    #[test]
    fn frame_range_page_aligned_region_is_exact() {
        let r = usable_frame_range(4096 * 5, 4096 * 3, 4096);
        assert_eq!(r, 5..8);
    }

    #[test]
    fn frame_range_shorter_than_one_frame_is_empty() {
        let r = usable_frame_range(0, 100, 4096);
        assert_eq!(r, 0..0);
        assert_eq!(r.count(), 0);
    }
}
