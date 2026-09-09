//! The pure rendezvous state machine behind `tarnos-kernel`'s
//! `ipc::endpoint::Endpoint` — extracted so it's unit-testable without a
//! lock, an interrupt-disabling `SpinLock`, or any hardware type. The
//! kernel's real `Endpoint` is a thin wrapper: a [`Slot`] behind its
//! `sync::SpinLock`, with `M` and `P` instantiated as its actual message
//! and process-id types.
//!
//! Generic over `M` (the message payload), `P` (however a waiting
//! process is identified), and a sender-queue capacity `N` (see
//! [`Slot`]'s doc comment for why only the sender side is a queue).
use crate::ring::RingBuffer;

/// Whoever is waiting on one side of a rendezvous.
pub enum Waiter<P> {
    Task(core::task::Waker),
    Process(P),
}

/// Outcome of [`Slot::try_send`].
pub enum SendOutcome<P> {
    /// A receiver was already waiting; here it is, to be woken/resumed.
    /// A [`Slot`] has no notion of a scheduler or executor, so actually
    /// doing that — and, for a `Waiter::Process`, retrieving the message
    /// this call left queued for it — is the caller's job.
    Delivered(Waiter<P>),
    /// No receiver was ready; `sender` is now queued and should block.
    Enqueued,
    /// No receiver was ready, and the sender queue was already at
    /// capacity — `sender` was *not* queued. The caller must report
    /// this as a real failure (e.g. a resource-exhaustion error)
    /// instead of silently dropping the send or blocking forever with
    /// no way to ever be woken.
    QueueFull,
}

/// Outcome of [`Slot::try_recv`].
pub enum RecvOutcome<M, P> {
    /// A sender was already waiting; `message` is the payload it sent,
    /// and `sender` must be woken/resumed to let it know delivery
    /// succeeded (it gets no data back, just success).
    Delivered { message: M, sender: Waiter<P> },
    /// No sender was ready; `receiver` is now queued and should block.
    Enqueued,
    /// A receiver was *already* waiting — see [`Slot`]'s doc comment on
    /// why the receiver side has no queue at all (capacity exactly 1).
    QueueFull,
}

/// A rendezvous point. The sender side is a bounded FIFO queue (see
/// [`SendOutcome::QueueFull`]) — two different processes (or one process
/// calling `send` twice before either is received) both waiting to send
/// is a completely ordinary scenario, not a caller bug. Earlier versions
/// of this type modeled only one waiting sender and panicked on a
/// second, which was a standing kernel-crashable-by-any-process bug (see
/// `docs/adr` and the milestone-2 plan) this queue fixes.
///
/// The receiver side stays a single value, deliberately not a queue,
/// for a reason with no clean fix short of giving every `Future` a
/// shared result cell: a woken `Waiter::Task` can only ever retrieve a
/// value by being re-polled and calling `try_recv`/`try_send` again —
/// a `Waker` carries no payload — so delivering to a queued *task*
/// receiver necessarily means putting the message back where that
/// specific re-poll will find it (see [`Slot::try_send`]'s
/// `ReceiverWaiting` arm). With more than one receiver queued, a second
/// waiting receiver could easily steal a message meant for the first by
/// re-polling sooner. A `Waiter::Process` receiver doesn't strictly need
/// this restriction (the caller resolves it synchronously, no re-poll
/// involved — see `tarnos-kernel`'s `Endpoint::wake_receiver`), but the
/// two waiter kinds share one `Slot`, so the stricter case governs. A
/// second receiver arriving while one is already queued reports
/// [`RecvOutcome::QueueFull`] instead of panicking, exactly like the
/// sender side at capacity.
pub enum Slot<M, P, const N: usize> {
    Empty,
    SendersWaiting(RingBuffer<(M, Waiter<P>), N>),
    ReceiverWaiting(Waiter<P>),
}

impl<M, P, const N: usize> Slot<M, P, N> {
    pub const fn new() -> Self {
        Slot::Empty
    }

    /// Attempts to deliver `message` right now via the waiting receiver,
    /// if any — see the type-level doc comment on why delivering to a
    /// receiver actually means re-queuing `(message, sender)` for that
    /// receiver to pick up, rather than handing it over directly.
    /// Otherwise queues `(message, sender)` as a new waiting sender.
    pub fn try_send(&mut self, message: M, sender: Waiter<P>) -> SendOutcome<P> {
        match self {
            Slot::ReceiverWaiting(_) => {
                let Slot::ReceiverWaiting(receiver) = core::mem::replace(self, Slot::Empty)
                else {
                    unreachable!()
                };
                let mut queue = RingBuffer::new();
                queue.push((message, sender));
                *self = Slot::SendersWaiting(queue);
                SendOutcome::Delivered(receiver)
            }
            Slot::Empty => {
                let mut queue = RingBuffer::new();
                queue.push((message, sender));
                *self = Slot::SendersWaiting(queue);
                SendOutcome::Enqueued
            }
            Slot::SendersWaiting(queue) => {
                if queue.push((message, sender)) {
                    SendOutcome::Enqueued
                } else {
                    SendOutcome::QueueFull
                }
            }
        }
    }

    /// Attempts to receive right now by taking the oldest waiting
    /// sender's message. If none is waiting, queues `receiver` instead —
    /// unless a receiver is *already* queued, in which case this reports
    /// [`RecvOutcome::QueueFull`] rather than displacing or panicking.
    pub fn try_recv(&mut self, receiver: Waiter<P>) -> RecvOutcome<M, P> {
        match self {
            Slot::SendersWaiting(queue) => {
                let (message, sender) = queue.pop().expect("SendersWaiting is never left empty");
                if queue.is_empty() {
                    *self = Slot::Empty;
                }
                RecvOutcome::Delivered { message, sender }
            }
            Slot::Empty => {
                *self = Slot::ReceiverWaiting(receiver);
                RecvOutcome::Enqueued
            }
            Slot::ReceiverWaiting(_) => RecvOutcome::QueueFull,
        }
    }
}

impl<M, P, const N: usize> Default for Slot<M, P, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{RecvOutcome, SendOutcome, Slot, Waiter};
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

    fn assert_process(w: Waiter<u32>, expected: u32) {
        match w {
            Waiter::Process(p) => assert_eq!(p, expected),
            Waiter::Task(_) => panic!("expected a Waiter::Process({expected})"),
        }
    }

    #[test]
    fn sender_first_then_receiver() {
        let mut slot: Slot<&str, u32, 4> = Slot::new();
        assert!(matches!(
            slot.try_send("hello", Waiter::Process(1)),
            SendOutcome::Enqueued
        ));
        match slot.try_recv(Waiter::Process(2)) {
            RecvOutcome::Delivered { message, sender } => {
                assert_eq!(message, "hello");
                assert_process(sender, 1);
            }
            _ => panic!("expected Delivered"),
        }
    }

    #[test]
    fn receiver_first_then_sender() {
        let mut slot: Slot<&str, u32, 4> = Slot::new();
        assert!(matches!(
            slot.try_recv(Waiter::Process(2)),
            RecvOutcome::Enqueued
        ));
        match slot.try_send("hello", Waiter::Process(1)) {
            SendOutcome::Delivered(receiver) => assert_process(receiver, 2),
            _ => panic!("expected Delivered"),
        }
        // The type-level doc comment's whole point: after being told a
        // receiver was displaced, the message must still be retrievable
        // via a normal try_recv (this is how a woken Task actually gets
        // it on re-poll; here we just simulate that re-poll directly).
        match slot.try_recv(Waiter::Process(99)) {
            RecvOutcome::Delivered { message, sender } => {
                assert_eq!(message, "hello");
                assert_process(sender, 1);
            }
            _ => panic!("expected the re-queued message to still be there"),
        }
    }

    #[test]
    fn receiver_first_wakes_the_waiting_task_on_send() {
        let mut slot: Slot<&str, u32, 4> = Slot::new();
        let (flag, receiver) = flag_waiter();
        assert!(matches!(slot.try_recv(receiver), RecvOutcome::Enqueued));
        match slot.try_send("hi", Waiter::Process(1)) {
            SendOutcome::Delivered(Waiter::Task(waker)) => waker.wake(),
            _ => panic!("expected a delivered task waiter"),
        }
        assert!(flag.0.load(Ordering::SeqCst), "receiver's waker must fire");
    }

    #[test]
    fn sender_first_wakes_the_waiting_task_on_recv() {
        let mut slot: Slot<&str, u32, 4> = Slot::new();
        let (flag, sender) = flag_waiter();
        assert!(matches!(slot.try_send("hi", sender), SendOutcome::Enqueued));
        match slot.try_recv(Waiter::Process(2)) {
            RecvOutcome::Delivered {
                sender: Waiter::Task(waker),
                ..
            } => waker.wake(),
            _ => panic!("expected a delivered task waiter"),
        }
        assert!(flag.0.load(Ordering::SeqCst), "sender's waker must fire");
    }

    #[test]
    fn two_senders_both_queue_instead_of_panicking() {
        // The historical bug this queue exists to fix: two different
        // processes (or one process retried before either was
        // delivered) both calling send with nobody receiving used to
        // panic the whole kernel on the second call. Now both simply
        // queue, FIFO, and are delivered to receivers in the order they
        // arrived.
        let mut slot: Slot<&str, u32, 4> = Slot::new();
        assert!(matches!(
            slot.try_send("first", Waiter::Process(1)),
            SendOutcome::Enqueued
        ));
        assert!(matches!(
            slot.try_send("second", Waiter::Process(2)),
            SendOutcome::Enqueued
        ));

        match slot.try_recv(Waiter::Process(10)) {
            RecvOutcome::Delivered { message, sender } => {
                assert_eq!(message, "first");
                assert_process(sender, 1);
            }
            _ => panic!("expected Delivered"),
        }
        match slot.try_recv(Waiter::Process(11)) {
            RecvOutcome::Delivered { message, sender } => {
                assert_eq!(message, "second");
                assert_process(sender, 2);
            }
            _ => panic!("expected Delivered"),
        }
    }

    #[test]
    fn sender_queue_full_reports_queue_full_not_panic() {
        let mut slot: Slot<&str, u32, 2> = Slot::new();
        assert!(matches!(
            slot.try_send("a", Waiter::Process(1)),
            SendOutcome::Enqueued
        ));
        assert!(matches!(
            slot.try_send("b", Waiter::Process(2)),
            SendOutcome::Enqueued
        ));
        assert!(matches!(
            slot.try_send("c", Waiter::Process(3)),
            SendOutcome::QueueFull
        ));
    }

    #[test]
    fn second_receiver_reports_queue_full_not_panic() {
        let mut slot: Slot<&str, u32, 4> = Slot::new();
        assert!(matches!(
            slot.try_recv(Waiter::Process(1)),
            RecvOutcome::Enqueued
        ));
        assert!(matches!(
            slot.try_recv(Waiter::Process(2)),
            RecvOutcome::QueueFull
        ));
    }
}
