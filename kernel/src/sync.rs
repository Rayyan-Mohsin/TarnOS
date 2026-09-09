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
/// the core deadlocks against itself (there is no second core to make
/// progress and release it). Disabling interrupts across the critical
/// section makes that reentrancy impossible instead of merely unlikely.
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

/// Placeholder for per-CPU state. A no-op on today's single-core kernel;
/// exists so call sites that will need real per-CPU indirection (a GS-base
/// pointer or an array indexed by APIC ID) once SMP lands are written
/// against this type now instead of a bare global.
pub struct PerCpu<T>(T);

impl<T> PerCpu<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    pub fn get(&self) -> &T {
        &self.0
    }
}
