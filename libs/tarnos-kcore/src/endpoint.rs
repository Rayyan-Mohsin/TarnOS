//! The pure rendezvous state machine behind `tarnos-kernel`'s
//! `ipc::endpoint::Endpoint` — extracted so it's unit-testable without a
//! lock, an interrupt-disabling `SpinLock`, or any hardware type. The
//! kernel's real `Endpoint` is a thin wrapper: a [`Slot`] behind its
//! `sync::SpinLock`, with `M` and `P` instantiated as its actual message
//! and process-id types.
//!
//! Generic over `M` (the message payload) and `P` (however a waiting
//! process is identified) so this crate never needs to know about
//! `tarnos-kernel`'s `MessagePayload` or `Pid` types.
use core::task::Waker;

/// Whoever is waiting on one side of a rendezvous.
pub enum Waiter<P> {
    Task(Waker),
    Process(P),
}

impl<P> Waiter<P> {
    /// Wakes a `Task` waiter immediately. `Process` waiters cannot be
    /// woken here — this type doesn't know about a scheduler — so the
    /// caller of the operation that produced a `Process` waiter is
    /// responsible for re-checking and resuming it.
    pub fn wake_if_task(self) {
        if let Waiter::Task(waker) = self {
            waker.wake();
        }
    }
}

/// A single-slot rendezvous: at most one waiting sender, or at most one
/// waiting receiver, never both and never more than one of either. A
/// second sender arriving while one is already waiting is a caller bug
/// (see [`Slot::try_send`]'s panic) rather than something this type
/// queues — deliberately, so a process can never make the kernel buffer
/// unbounded messages just by sending faster than anyone reads.
pub enum Slot<M, P> {
    Empty,
    SenderWaiting { message: M, sender: Waiter<P> },
    ReceiverWaiting { receiver: Waiter<P> },
}

impl<M, P> Slot<M, P> {
    pub const fn new() -> Self {
        Slot::Empty
    }

    /// Attempts to deliver `message` right now. If a receiver is already
    /// waiting, hands off and wakes it (if it's a task — a waiting
    /// process is resumed by whoever dequeues it, since this type cannot
    /// touch a scheduler), returning `true`. Otherwise records `sender`
    /// as the new waiting sender and returns `false`.
    ///
    /// Panics if a sender is already waiting — see the type-level doc
    /// comment on why this is a single-slot rendezvous, not a queue.
    pub fn try_send(&mut self, message: M, sender: Waiter<P>) -> bool {
        match self {
            Slot::SenderWaiting { .. } => {
                panic!("Slot::try_send called while another sender is already waiting")
            }
            Slot::ReceiverWaiting { .. } => {
                let Slot::ReceiverWaiting { receiver } =
                    core::mem::replace(self, Slot::SenderWaiting { message, sender })
                else {
                    unreachable!()
                };
                // The message now waits in the slot for the receiver to
                // actually take on its next poll; waking it here only
                // re-schedules that poll (for a task) or is a no-op for
                // a process waiter (see `Waiter::wake_if_task`).
                receiver.wake_if_task();
                true
            }
            Slot::Empty => {
                *self = Slot::SenderWaiting { message, sender };
                false
            }
        }
    }

    /// Attempts to receive right now. If a sender is already waiting,
    /// takes its message and wakes it (if a task). Otherwise records
    /// `receiver` as the new waiting receiver and returns `None`.
    ///
    /// Panics if a receiver is already waiting, mirroring
    /// [`Slot::try_send`]'s panic on the sender side.
    pub fn try_recv(&mut self, receiver: Waiter<P>) -> Option<M> {
        match self {
            Slot::ReceiverWaiting { .. } => {
                panic!("Slot::try_recv called while another receiver is already waiting")
            }
            Slot::SenderWaiting { .. } => {
                let Slot::SenderWaiting { message, sender } =
                    core::mem::replace(self, Slot::Empty)
                else {
                    unreachable!()
                };
                sender.wake_if_task();
                Some(message)
            }
            Slot::Empty => {
                *self = Slot::ReceiverWaiting { receiver };
                None
            }
        }
    }

    /// Receives only if a sender is already waiting; never registers a
    /// waiting receiver otherwise. For callers (like a non-blocking
    /// syscall path) that can't usefully suspend themselves here.
    pub fn try_recv_nonblocking(&mut self) -> Option<M> {
        match self {
            Slot::SenderWaiting { .. } => {
                let Slot::SenderWaiting { message, sender } =
                    core::mem::replace(self, Slot::Empty)
                else {
                    unreachable!()
                };
                sender.wake_if_task();
                Some(message)
            }
            _ => None,
        }
    }
}

impl<M, P> Default for Slot<M, P> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{Slot, Waiter};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::Wake;

    struct FlagWaker(AtomicBool);
    impl Wake for FlagWaker {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn flag_waiter() -> (Arc<FlagWaker>, Waiter<u32>) {
        let flag = Arc::new(FlagWaker(AtomicBool::new(false)));
        let waker = std::task::Waker::from(flag.clone());
        (flag, Waiter::Task(waker))
    }

    #[test]
    fn sender_first_then_receiver() {
        let mut slot: Slot<&str, u32> = Slot::new();
        assert!(!slot.try_send("hello", Waiter::Process(1)));
        assert_eq!(slot.try_recv(Waiter::Process(2)), Some("hello"));
    }

    #[test]
    fn receiver_first_then_sender() {
        let mut slot: Slot<&str, u32> = Slot::new();
        assert_eq!(slot.try_recv(Waiter::Process(2)), None);
        assert!(slot.try_send("hello", Waiter::Process(1)));
    }

    #[test]
    fn receiver_first_wakes_the_waiting_task_on_send() {
        let mut slot: Slot<&str, u32> = Slot::new();
        let (flag, receiver) = flag_waiter();
        assert_eq!(slot.try_recv(receiver), None);
        assert!(!flag.0.load(Ordering::SeqCst));
        slot.try_send("hi", Waiter::Process(1));
        assert!(flag.0.load(Ordering::SeqCst), "receiver's waker must fire");
    }

    #[test]
    fn sender_first_wakes_the_waiting_task_on_recv() {
        let mut slot: Slot<&str, u32> = Slot::new();
        let (flag, sender) = flag_waiter();
        assert!(!slot.try_send("hi", sender));
        assert!(!flag.0.load(Ordering::SeqCst));
        slot.try_recv(Waiter::Process(2));
        assert!(flag.0.load(Ordering::SeqCst), "sender's waker must fire");
    }

    #[test]
    fn nonblocking_recv_takes_a_waiting_sender_without_blocking() {
        let mut slot: Slot<&str, u32> = Slot::new();
        assert_eq!(slot.try_recv_nonblocking(), None);
        slot.try_send("hi", Waiter::Process(1));
        assert_eq!(slot.try_recv_nonblocking(), Some("hi"));
    }

    #[test]
    fn nonblocking_recv_never_registers_as_a_waiting_receiver() {
        let mut slot: Slot<&str, u32> = Slot::new();
        assert_eq!(slot.try_recv_nonblocking(), None);
        // Had this registered a waiting receiver, a subsequent send
        // would return `true` (delivered) instead of `false` (enqueued).
        assert!(!slot.try_send("hi", Waiter::Process(1)));
    }

    #[test]
    #[should_panic(expected = "another sender is already waiting")]
    fn second_send_with_no_receiver_panics_today() {
        // Pinned as documentation of the current, deliberately-not-yet-
        // fixed behavior: a process holding nothing more than a SEND
        // capability can panic the whole kernel by sending twice with
        // nobody receiving (see docs/adr and the milestone-2 plan). This
        // test is expected to flip from should_panic to a real
        // assertion of correct blocking/queueing once that lands.
        let mut slot: Slot<&str, u32> = Slot::new();
        assert!(!slot.try_send("first", Waiter::Process(1)));
        slot.try_send("second", Waiter::Process(2));
    }

    #[test]
    #[should_panic(expected = "another receiver is already waiting")]
    fn second_recv_with_no_sender_panics_today() {
        let mut slot: Slot<&str, u32> = Slot::new();
        assert_eq!(slot.try_recv(Waiter::Process(1)), None);
        slot.try_recv(Waiter::Process(2));
    }
}
