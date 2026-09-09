//! Cooperative executor for kernel-space futures (driver state machines,
//! the future console server).
//!
//! Deliberately separate from the (future) preemptive process scheduler:
//! kernel tasks are trusted and stackless, so they don't need forced
//! preemption or a full register/stack context switch the way untrusted
//! usermode code does. The two share one bridge primitive — a `Waker`
//! that ends up here, in the ready queue — rather than one unified
//! run-loop; see `docs/adr/0002-async-executor-and-scheduler.md`.
//!
//! The load-bearing rule: an interrupt handler may only ever *enqueue*
//! (call [`Waker::wake`]) — never poll a future or run task code inline.
//! That's what makes the ready queue a fixed-capacity array behind a
//! [`SpinLock`](crate::sync::SpinLock) instead of anything backed by the
//! heap: the global allocator's own lock is not interrupt-reentrant, so
//! an interrupt handler that indirectly tried to allocate while normal
//! code held that lock would deadlock the core against itself.
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::task::Wake;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::task::{Context, Poll, Waker};

use crate::sync::SpinLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TaskId(u64);

impl TaskId {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        TaskId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

pub struct Task {
    id: TaskId,
    future: Pin<Box<dyn Future<Output = ()>>>,
}

impl Task {
    pub fn new(future: impl Future<Output = ()> + 'static) -> Self {
        Self {
            id: TaskId::new(),
            future: Box::pin(future),
        }
    }

    fn poll(&mut self, context: &mut Context) -> Poll<()> {
        self.future.as_mut().poll(context)
    }
}

const READY_QUEUE_CAPACITY: usize = 64;

struct ReadyQueue {
    buffer: [Option<TaskId>; READY_QUEUE_CAPACITY],
    head: usize,
    len: usize,
}

impl ReadyQueue {
    const fn new() -> Self {
        Self {
            buffer: [None; READY_QUEUE_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, id: TaskId) -> bool {
        if self.len == READY_QUEUE_CAPACITY {
            return false;
        }
        let idx = (self.head + self.len) % READY_QUEUE_CAPACITY;
        self.buffer[idx] = Some(id);
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<TaskId> {
        if self.len == 0 {
            return None;
        }
        let id = self.buffer[self.head].take();
        self.head = (self.head + 1) % READY_QUEUE_CAPACITY;
        self.len -= 1;
        id
    }
}

static READY_QUEUE: SpinLock<ReadyQueue> = SpinLock::new(ReadyQueue::new());

/// Set when the ready queue is full at wake time, so the woken task ID is
/// lost. Recovered from by conservatively re-polling every live task on
/// the next drain instead of a panic or an allocation from interrupt
/// context — correctness over precision for an event that should be rare
/// (64 simultaneously-ready kernel tasks) and is not fatal to recover
/// from.
static OVERFLOWED: AtomicBool = AtomicBool::new(false);

struct TaskWaker {
    task_id: TaskId,
}

impl TaskWaker {
    fn new_waker(task_id: TaskId) -> Waker {
        Waker::from(Arc::new(TaskWaker { task_id }))
    }

    fn wake_task(&self) {
        if !READY_QUEUE.lock().push(self.task_id) {
            OVERFLOWED.store(true, Ordering::Relaxed);
        }
    }
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_task();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wake_task();
    }
}

/// A single-threaded, cooperative task executor. Not itself behind a
/// global lock — it is only ever driven from one place (the kernel idle
/// loop), never from interrupt context, so it doesn't need one.
pub struct Executor {
    tasks: BTreeMap<TaskId, Task>,
    wakers: BTreeMap<TaskId, Waker>,
}

impl Executor {
    pub fn new() -> Self {
        Self {
            tasks: BTreeMap::new(),
            wakers: BTreeMap::new(),
        }
    }

    /// Adds a task and schedules it to run at least once.
    pub fn spawn(&mut self, task: Task) {
        let id = task.id;
        self.tasks.insert(id, task);
        READY_QUEUE.lock().push(id);
    }

    /// Polls every currently-ready task once. Safe to call from the
    /// kernel idle loop after returning from `hlt` — never from inside an
    /// interrupt handler itself, since polling a future can allocate
    /// (e.g. inserting into `wakers` below) and interrupts are exactly
    /// the context that must never touch the heap allocator's lock.
    pub fn run_ready_tasks(&mut self) {
        if OVERFLOWED.swap(false, Ordering::Relaxed) {
            let ids: Vec<TaskId> = self.tasks.keys().copied().collect();
            for id in ids {
                self.poll_task(id);
            }
            return;
        }

        while let Some(id) = READY_QUEUE.lock().pop() {
            self.poll_task(id);
        }
    }

    fn poll_task(&mut self, id: TaskId) {
        let Some(task) = self.tasks.get_mut(&id) else {
            return; // already completed and removed
        };
        let waker = self
            .wakers
            .entry(id)
            .or_insert_with(|| TaskWaker::new_waker(id))
            .clone();
        let mut context = Context::from_waker(&waker);
        if task.poll(&mut context).is_ready() {
            self.tasks.remove(&id);
            self.wakers.remove(&id);
        }
    }
}
