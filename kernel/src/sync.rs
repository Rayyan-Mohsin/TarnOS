//! Synchronization primitives with the one property `spin`'s own types
//! don't give us: safety against a lock held by normal code being
//! re-entered by an interrupt handler on the same core. Locks that never
//! cross an interrupt boundary (GDT/IDT setup, the executor's own task
//! map) use plain `spin::{Mutex, Once}` directly; anything an interrupt
//! handler might also touch (the executor's ready queue, a driver's
//! registered `Waker`) uses [`SpinLock`] here instead.
use core::ops::{Deref, DerefMut};
use x86_64::instructions::interrupts;

/// A spinlock that disables interrupts for the duration it is held.
///
/// Without this, code on this core holding a plain spinlock could be
/// interrupted, and if the interrupt handler tries to take the same lock,
/// this core deadlocks against itself — a second core spinning on the
/// same lock would eventually make progress once the first core's
/// interrupt handler returns and releases it, but a same-core interrupt
/// handler never returns until it acquires the lock it's stuck waiting
/// on. Disabling interrupts across the critical section makes that
/// same-core reentrancy impossible instead of merely unlikely; the
/// underlying `spin::Mutex`'s ordinary cross-core mutual exclusion
/// already handles any number of cores correctly on its own.
pub struct SpinLock<T> {
    inner: spin::Mutex<T>,
}

pub struct SpinLockGuard<'a, T> {
    // `Option` so `Drop` can release the inner lock *before* restoring
    // interrupts, rather than after — the two must not be reordered, or
    // there is a window where interrupts are enabled but the lock is
    // still held, which is the exact reentrancy hazard this type exists
    // to prevent.
    guard: Option<spin::MutexGuard<'a, T>>,
    interrupts_were_enabled: bool,
}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: spin::Mutex::new(value),
        }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let interrupts_were_enabled = interrupts::are_enabled();
        if interrupts_were_enabled {
            interrupts::disable();
        }
        SpinLockGuard {
            guard: Some(self.inner.lock()),
            interrupts_were_enabled,
        }
    }
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard.as_ref().expect("guard taken before drop")
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_mut().expect("guard taken before drop")
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.guard.take();
        if self.interrupts_were_enabled {
            interrupts::enable();
        }
    }
}
