//! Synchronous, rendezvous-style IPC — the seL4/L4-style primitive that
//! is the microkernel's actual mechanism, not just a name for "message
//! queue." A send does not complete until a receiver is actually present:
//! there is no unbounded kernel-buffered queue a process could grow to
//! exhaust kernel memory. See
//! `docs/adr/0003-ipc-message-format.md` for the full rationale.
//!
//! Both sides can wait either as a kernel task (a `Waker`, driven by the
//! executor) or as a process (a `Pid`, driven by the scheduler).
//! `Endpoint` itself only records which kind of waiter is pending and
//! delivers to it; actually suspending a *process* until then would be
//! the syscall layer's job (`arch::x86_64::syscall`), while a *task*
//! suspends itself simply by returning [`core::task::Poll::Pending`]
//! from its `Future::poll`. This milestone's syscall path only ever
//! calls [`Endpoint::try_send`] (`sender_pid` names who to record if no
//! receiver is waiting, but nothing yet re-wakes a blocked sender
//! process) and the non-blocking [`Endpoint::try_recv_nonblocking`] —
//! actually blocking a process on `recv` needs the scheduler to suspend
//! and later resume it, which nothing in this milestone's demo exercises
//! (the demo's boot ordering guarantees a receiver is always already
//! waiting, so a process never actually needs to block on `send` either).
//!
//! The actual rendezvous state machine is `tarnos_kcore::endpoint::Slot`
//! — extracted so it's unit-testable on the host without a lock. This
//! type is a thin wrapper: a `Slot<MessagePayload, Pid>` behind the
//! kernel's interrupt-disabling `SpinLock`.
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use tarnos_abi::Message;
use tarnos_kcore::endpoint::Slot;

use crate::sync::SpinLock;
use crate::task::Pid;

use super::message::MessagePayload;

/// Whoever is waiting on one side of a rendezvous — see
/// `tarnos_kcore::endpoint::Waiter`'s doc comment.
pub type Waiter = tarnos_kcore::endpoint::Waiter<Pid>;

pub struct Endpoint {
    slot: SpinLock<Slot<MessagePayload, Pid>>,
}

impl Endpoint {
    pub fn new() -> Self {
        Self {
            slot: SpinLock::new(Slot::new()),
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
        self.slot
            .lock()
            .try_send(MessagePayload::inline(message), Waiter::Process(sender_pid))
    }

    /// Non-blocking receive for the syscall path: returns a message if a
    /// sender is already waiting, or `None` without registering any
    /// waiter otherwise. A process cannot usefully "wait" here yet — see
    /// [`Waiter::Process`]'s note on how a blocked process would need
    /// the scheduler's cooperation to be woken later, which nothing in
    /// this milestone's demo exercises — so this never blocks; it only
    /// checks.
    pub fn try_recv_nonblocking(&self) -> Option<Message> {
        self.slot
            .lock()
            .try_recv_nonblocking()
            .map(MessagePayload::into_inline)
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
                if self.endpoint.slot.lock().try_send(
                    MessagePayload::inline(message),
                    Waiter::Task(cx.waker().clone()),
                ) {
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
        match self
            .endpoint
            .slot
            .lock()
            .try_recv(Waiter::Task(cx.waker().clone()))
        {
            Some(message) => Poll::Ready(message.into_inline()),
            None => Poll::Pending,
        }
    }
}
