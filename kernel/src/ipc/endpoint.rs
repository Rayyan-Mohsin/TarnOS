//! Synchronous, rendezvous-style IPC — the seL4/L4-style primitive that
//! is the microkernel's actual mechanism, not just a name for "message
//! queue." A send does not complete until a receiver is actually present:
//! there is no unbounded kernel-buffered queue a process could grow to
//! exhaust kernel memory. See
//! `docs/adr/0003-ipc-message-format.md` for the full rationale, and
//! `docs/adr/0005-fault-isolation-and-blocking-ipc.md` for how blocking
//! actually works.
//!
//! Both sides can wait either as a kernel task (a `Waker`, driven by the
//! executor) or as a process (a `Pid`, driven by the scheduler).
//! `Endpoint` records waiters in `tarnos_kcore::endpoint::Slot` (a
//! bounded sender queue sized to `task::scheduler::MAX_PROCESSES`, plus
//! a single-value receiver side — see that type's doc comment for why
//! the two sides aren't symmetric) and delivers FIFO. A `Task` waiter
//! resumes itself by being woken through the ordinary `Future`/`Waker`
//! mechanism, which only ever triggers a re-poll — so delivering to one
//! actually means re-queuing the message where that re-poll's own
//! `try_recv`/`try_send` call will find it (see `Slot::try_send`'s doc).
//! A `Process` waiter has no poll loop of its own, so [`Endpoint`]
//! resolves it synchronously instead: `wake_receiver` immediately calls
//! back into [`Endpoint::try_recv`] on that process's behalf the moment
//! it's told a message was queued for it, then hands the result to
//! `task::scheduler::wake_blocked_process`. Actually suspending a
//! process in the first place is `arch::x86_64::syscall`'s job
//! (`sys_send`/`sys_recv` call `task::scheduler::block_current_process`
//! when this module reports no partner was ready) — this module has no
//! notion of a scheduler itself.
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use tarnos_abi::Message;
use tarnos_kcore::endpoint::{RecvOutcome, SendOutcome};

use crate::sync::SpinLock;
use crate::task::scheduler::{self, WakeResult, MAX_PROCESSES};
use crate::task::Pid;

use super::message::MessagePayload;

/// Whoever is waiting on one side of a rendezvous — see
/// `tarnos_kcore::endpoint::Waiter`'s doc comment.
pub type Waiter = tarnos_kcore::endpoint::Waiter<Pid>;

type Slot = tarnos_kcore::endpoint::Slot<MessagePayload, Pid, MAX_PROCESSES>;

/// Outcome of a syscall-path send — see `Endpoint::try_send`.
pub enum SendResult {
    Delivered,
    /// No receiver was ready; the calling process has been queued and
    /// must actually block (`task::scheduler::block_current_process`).
    Blocked,
    /// The sender queue was already at `MAX_PROCESSES` capacity — every
    /// process that could possibly exist is already queued here, so the
    /// caller should report a resource-exhaustion error rather than
    /// block with no way to ever be woken.
    QueueFull,
}

/// Outcome of a syscall-path receive — see `Endpoint::try_recv`.
pub enum RecvResult {
    Delivered(Message),
    Blocked,
    /// A receiver (task or process) was already waiting on this
    /// endpoint — see `tarnos_kcore::endpoint::Slot`'s doc comment on
    /// why the receiver side's capacity is exactly one.
    QueueFull,
}

/// Wakes whichever kind of waiter was just displaced by a successful
/// receive. A woken sender gets no data back, just success.
/// `Waiter::None` (see its doc comment) means the original sender
/// already got its answer synchronously and there is nothing to do.
fn wake_sender(sender: Waiter) {
    match sender {
        Waiter::Task(waker) => waker.wake(),
        Waiter::Process(pid) => scheduler::wake_blocked_process(pid, WakeResult::SendCompleted),
        Waiter::None => {}
    }
}

pub struct Endpoint {
    slot: SpinLock<Slot>,
}

impl Endpoint {
    pub fn new() -> Self {
        Self {
            slot: SpinLock::new(Slot::new()),
        }
    }

    /// Wakes whichever kind of waiter [`Slot::try_send`] just displaced.
    /// A `Task` is woken via its `Waker`, which triggers a re-poll that
    /// calls `try_recv`/`try_send` again to actually retrieve/complete —
    /// the only way a `Future` can receive a value, since a `Waker`
    /// carries no payload. A `Process` has no poll loop to do that
    /// itself, so this performs the equivalent `try_recv` synchronously,
    /// right here, on its behalf — safe to call with `self.slot`
    /// unlocked (every caller below drops its lock guard, via a `let`
    /// binding, before reaching this) since that's exactly what this
    /// method itself needs to lock again.
    fn wake_receiver(&self, receiver: Waiter) {
        match receiver {
            Waiter::Task(waker) => waker.wake(),
            Waiter::Process(pid) => {
                if let RecvResult::Delivered(message) = self.try_recv(pid) {
                    scheduler::wake_blocked_process(
                        pid,
                        WakeResult::RecvCompleted {
                            tag: message.tag,
                            words: [message.words[0], message.words[1], message.words[2]],
                        },
                    );
                }
            }
            // `Slot::try_send` only ever returns a real Task/Process
            // receiver via `SendOutcome::Delivered` -- `Waiter::None` is
            // exclusively a *sender*-side placeholder (see its doc
            // comment) -- but the match must stay exhaustive.
            Waiter::None => {}
        }
    }

    /// Send for the syscall path. `Delivered` and `QueueFull` are both
    /// immediate outcomes; `Blocked` means the caller (`sys_send`) must
    /// now actually suspend the calling process.
    pub fn try_send(&self, message: Message, sender_pid: Pid) -> SendResult {
        // Bound to `outcome` rather than matched on directly: the
        // `SpinLockGuard` `.lock()` returns must be dropped (at the end
        // of this `let` statement) before `wake_receiver` below, which
        // may need to lock `self.slot` again — a `match self.slot.lock()...`
        // would keep the temporary guard alive for the whole match,
        // across that call, and self-deadlock the core.
        let outcome = self
            .slot
            .lock()
            .try_send(MessagePayload::inline(message), Waiter::Process(sender_pid));
        match outcome {
            SendOutcome::Delivered(receiver) => {
                self.wake_receiver(receiver);
                SendResult::Delivered
            }
            SendOutcome::Enqueued => SendResult::Blocked,
            SendOutcome::QueueFull => SendResult::QueueFull,
        }
    }

    /// Receive for the syscall path. Mirrors [`Endpoint::try_send`] —
    /// see its doc comment on why the outcome is bound via `let` before
    /// being matched on.
    pub fn try_recv(&self, receiver_pid: Pid) -> RecvResult {
        let outcome = self.slot.lock().try_recv(Waiter::Process(receiver_pid));
        match outcome {
            RecvOutcome::Delivered { message, sender } => {
                wake_sender(sender);
                RecvResult::Delivered(message.into_inline())
            }
            RecvOutcome::Enqueued => RecvResult::Blocked,
            RecvOutcome::QueueFull => RecvResult::QueueFull,
        }
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
                // See Endpoint::try_send's doc comment on why this is a
                // `let` binding, not matched on directly.
                let outcome = self.endpoint.slot.lock().try_send(
                    MessagePayload::inline(message),
                    Waiter::Task(cx.waker().clone()),
                );
                match outcome {
                    SendOutcome::Delivered(receiver) => {
                        self.endpoint.wake_receiver(receiver);
                        Poll::Ready(())
                    }
                    SendOutcome::Enqueued => Poll::Pending,
                    SendOutcome::QueueFull => {
                        // All MAX_PROCESSES slots plus every other
                        // kernel task are already queued on this one
                        // endpoint -- vanishingly unlikely for this
                        // milestone's actual endpoints, each used by at
                        // most a couple of tasks/processes. Rather than
                        // register nothing and hang forever with no
                        // waker stored anywhere, retry on the next drain.
                        self.message = Some(message);
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
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
        // See Endpoint::try_send's doc comment on why this is a `let`
        // binding, not matched on directly.
        let outcome = self
            .endpoint
            .slot
            .lock()
            .try_recv(Waiter::Task(cx.waker().clone()));
        match outcome {
            RecvOutcome::Delivered { message, sender } => {
                wake_sender(sender);
                Poll::Ready(message.into_inline())
            }
            RecvOutcome::Enqueued => Poll::Pending,
            RecvOutcome::QueueFull => {
                // See SendFuture::poll's identical case.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}
