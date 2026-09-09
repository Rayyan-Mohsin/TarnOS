//! Synchronous, rendezvous-style IPC — the seL4/L4-style primitive that
//! is the microkernel's actual mechanism, not just a name for "message
//! queue." A send does not complete until a receiver is actually present:
//! there is no unbounded kernel-buffered queue a process could grow to
//! exhaust kernel memory. See
//! `docs/adr/0003-ipc-message-format.md` for the full rationale.
//!
//! Both sides can wait either as a kernel task (a `Waker`, driven by the
//! executor) or — once the scheduler exists — as a process (a `Pid`,
//! driven by the scheduler). `Endpoint` itself only records which kind of
//! waiter is pending and delivers to it; actually suspending a *process*
//! until then is the syscall layer's job; a *task* suspends itself simply
//! by returning [`core::task::Poll::Pending`] from its `Future::poll`.
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use tarnos_abi::Message;

use crate::sync::SpinLock;
use crate::task::Pid;

use super::message::MessagePayload;

/// Whoever is waiting on one side of a rendezvous.
pub enum Waiter {
    Task(Waker),
    Process(Pid),
}

impl Waiter {
    /// Wakes a `Task` waiter immediately. `Process` waiters cannot be
    /// woken here — `Endpoint` doesn't know about the scheduler — so the
    /// caller of the operation that produced a `Process` waiter (the
    /// syscall layer, once it exists) is responsible for re-checking and
    /// resuming it.
    fn wake_if_task(self) {
        if let Waiter::Task(waker) = self {
            waker.wake();
        }
    }
}

enum Slot {
    Empty,
    SenderWaiting {
        message: MessagePayload,
        sender: Waiter,
    },
    ReceiverWaiting {
        receiver: Waiter,
    },
}

pub struct Endpoint {
    slot: SpinLock<Slot>,
}

impl Endpoint {
    pub fn new() -> Self {
        Self {
            slot: SpinLock::new(Slot::Empty),
        }
    }

    /// Attempts to deliver `message` right now. If a receiver is already
    /// waiting, hands off and wakes it (if it's a task — a waiting
    /// process is resumed by whoever dequeues it, since `Endpoint` cannot
    /// touch the scheduler), returning `true`. Otherwise records `sender`
    /// as the endpoint's new waiting sender and returns `false`.
    ///
    /// Panics if a sender is already waiting — `Endpoint` is a
    /// single-slot rendezvous, and having two senders queued at once
    /// would silently drop one of them; callers must not call this twice
    /// without an intervening successful receive.
    fn try_send_inner(&self, message: Message, sender: Waiter) -> bool {
        let mut guard = self.slot.lock();
        match &*guard {
            Slot::SenderWaiting { .. } => {
                drop(guard);
                panic!("Endpoint::try_send called while another sender is already waiting")
            }
            Slot::ReceiverWaiting { .. } => {
                let Slot::ReceiverWaiting { receiver } =
                    core::mem::replace(
                        &mut *guard,
                        Slot::SenderWaiting {
                            message: MessagePayload::inline(message),
                            sender,
                        },
                    )
                else {
                    unreachable!()
                };
                // The message is now waiting in the slot for the receiver
                // to actually take on its next poll; waking it here only
                // re-schedules that poll (for a task) or is a no-op for
                // now (for a process — see `Waiter::wake_if_task`).
                drop(guard);
                receiver.wake_if_task();
                true
            }
            Slot::Empty => {
                *guard = Slot::SenderWaiting {
                    message: MessagePayload::inline(message),
                    sender,
                };
                false
            }
        }
    }

    /// Attempts to receive right now. If a sender is already waiting,
    /// takes its message, wakes it (if a task), and returns it.
    /// Otherwise records `receiver` as the endpoint's new waiting
    /// receiver and returns `None`.
    fn try_recv_inner(&self, receiver: Waiter) -> Option<Message> {
        let mut guard = self.slot.lock();
        match &*guard {
            Slot::ReceiverWaiting { .. } => {
                drop(guard);
                panic!("Endpoint::try_recv called while another receiver is already waiting")
            }
            Slot::SenderWaiting { .. } => {
                let Slot::SenderWaiting { message, sender } =
                    core::mem::replace(&mut *guard, Slot::Empty)
                else {
                    unreachable!()
                };
                drop(guard);
                sender.wake_if_task();
                Some(message.into_inline())
            }
            Slot::Empty => {
                *guard = Slot::ReceiverWaiting { receiver };
                None
            }
        }
    }

    /// Non-blocking send for the syscall path: a process never awaits a
    /// future, so this returns immediately either way. `true` means
    /// delivered; `false` means no receiver was waiting and the sending
    /// process has been recorded as the new waiting sender — suspending
    /// that process until it is dequeued is not yet implemented (no
    /// scheduler exists this milestone to suspend it against), so
    /// callers of this milestone's demo path rely on the receiver always
    /// being primed first (see `docs/adr` and the boot-ordering note in
    /// `main.rs`) and treat `false` as an error rather than a real block.
    pub fn try_send(&self, message: Message, sender_pid: Pid) -> bool {
        self.try_send_inner(message, Waiter::Process(sender_pid))
    }

    /// Async send for kernel-task callers: suspends the calling task
    /// (via the ordinary `Future`/`Waker` mechanism, not the scheduler)
    /// until a receiver takes the message.
    pub fn send(&self, message: Message) -> SendFuture<'_> {
        SendFuture {
            endpoint: self,
            message: Some(message),
        }
    }

    /// Async receive for kernel-task callers: suspends the calling task
    /// until a sender hands off a message.
    pub fn recv(&self) -> RecvFuture<'_> {
        RecvFuture { endpoint: self }
    }
}

impl Default for Endpoint {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SendFuture<'a> {
    endpoint: &'a Endpoint,
    message: Option<Message>,
}

impl Future for SendFuture<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match self.message.take() {
            Some(message) => {
                // First poll: attempt delivery now. If no receiver is
                // waiting yet, the message has just been moved into the
                // endpoint's pending-sender slot (not lost) and this
                // future has nothing left to do but wait to be told it
                // was taken.
                if self
                    .endpoint
                    .try_send_inner(message, Waiter::Task(cx.waker().clone()))
                {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
            None => {
                // We're only re-polled after `wake()`, which nothing
                // calls on a waiting sender's waker except a receiver
                // that has just taken the message out of the endpoint —
                // so reaching here at all means delivery already
                // happened.
                Poll::Ready(())
            }
        }
    }
}

pub struct RecvFuture<'a> {
    endpoint: &'a Endpoint,
}

impl Future for RecvFuture<'_> {
    type Output = Message;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Message> {
        match self.endpoint.try_recv_inner(Waiter::Task(cx.waker().clone())) {
            Some(message) => Poll::Ready(message),
            None => Poll::Pending,
        }
    }
}
