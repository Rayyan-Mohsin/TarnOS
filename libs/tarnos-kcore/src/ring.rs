//! A fixed-capacity FIFO ring buffer.
//!
//! Used anywhere a queue must never allocate — an interrupt handler
//! enqueuing a woken task/process id, for instance, where the global
//! heap allocator's lock is not safe to touch (see
//! `tarnos-kernel`'s `task::executor` and `task::scheduler`, both of
//! which had their own hand-copied version of exactly this type before
//! it was extracted here).

/// A FIFO queue over `N` slots, backed by a fixed-size array — no heap
/// allocation, ever. `push` on a full buffer fails rather than growing;
/// `pop` on an empty one returns `None` rather than blocking.
pub struct RingBuffer<T, const N: usize> {
    buffer: [Option<T>; N],
    head: usize,
    len: usize,
}

impl<T: Copy, const N: usize> RingBuffer<T, N> {
    pub const fn new() -> Self {
        Self {
            buffer: [None; N],
            head: 0,
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_full(&self) -> bool {
        self.len == N
    }

    /// Appends `value`. Returns `false` (and leaves the buffer
    /// unchanged) if it is already at capacity.
    pub fn push(&mut self, value: T) -> bool {
        if self.len == N {
            return false;
        }
        let idx = (self.head + self.len) % N;
        self.buffer[idx] = Some(value);
        self.len += 1;
        true
    }

    /// Removes and returns the oldest value, or `None` if empty.
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let value = self.buffer[self.head].take();
        self.head = (self.head + 1) % N;
        self.len -= 1;
        value
    }
}

impl<T: Copy, const N: usize> Default for RingBuffer<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::RingBuffer;

    #[test]
    fn empty_pop_returns_none() {
        let mut q: RingBuffer<u32, 4> = RingBuffer::new();
        assert!(q.is_empty());
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn fifo_order_is_preserved() {
        let mut q: RingBuffer<u32, 4> = RingBuffer::new();
        assert!(q.push(1));
        assert!(q.push(2));
        assert!(q.push(3));
        assert_eq!(q.pop(), Some(1));
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), Some(3));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn push_fails_when_full_without_corrupting_state() {
        let mut q: RingBuffer<u32, 2> = RingBuffer::new();
        assert!(q.push(1));
        assert!(q.push(2));
        assert!(q.is_full());
        assert!(!q.push(3), "push into a full buffer must fail");
        // The failed push must not have clobbered anything already queued.
        assert_eq!(q.pop(), Some(1));
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn wraps_around_the_backing_array() {
        let mut q: RingBuffer<u32, 3> = RingBuffer::new();
        // Push/pop enough times that `head` cycles past the end of the
        // backing array at least once, exercising the modulo wraparound
        // in both push and pop.
        for round in 0..5u32 {
            assert!(q.push(round * 10));
            assert!(q.push(round * 10 + 1));
            assert_eq!(q.pop(), Some(round * 10));
            assert_eq!(q.pop(), Some(round * 10 + 1));
            assert!(q.is_empty());
        }
    }

    #[test]
    fn len_tracks_pushes_and_pops() {
        let mut q: RingBuffer<u32, 4> = RingBuffer::new();
        assert_eq!(q.len(), 0);
        q.push(1);
        q.push(2);
        assert_eq!(q.len(), 2);
        q.pop();
        assert_eq!(q.len(), 1);
    }
}
