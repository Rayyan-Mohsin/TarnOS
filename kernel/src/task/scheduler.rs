//! Preemptible round-robin scheduler over processes.
//!
//! The process table and ready queue are both fixed-size arrays, not
//! `BTreeMap`/`VecDeque` — [`on_timer_tick`] runs with interrupts
//! disabled (it's called from the timer entry stub, see
//! `arch::x86_64::context_switch`), and the global heap allocator's own
//! lock is not interrupt-reentrant. A fixed-capacity design means the
//! hot preemption path never allocates, so it can never deadlock against
//! normal code caught mid-allocation when the timer fires. Process
//! *creation* ([`spawn`]) does allocate (`Box::new`), but only ever runs
//! from normal, non-interrupt code.
use alloc::boxed::Box;

use x86_64::VirtAddr;

use crate::arch::x86_64::context_switch::TrapFrame;
use crate::arch::x86_64::gdt;
use crate::sync::SpinLock;

use super::process::{Process, ProcessState};
use super::Pid;

const MAX_PROCESSES: usize = 16;

struct ReadyQueue {
    buffer: [Option<Pid>; MAX_PROCESSES],
    head: usize,
    len: usize,
}

impl ReadyQueue {
    const fn new() -> Self {
        Self {
            buffer: [None; MAX_PROCESSES],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, pid: Pid) -> bool {
        if self.len == MAX_PROCESSES {
            return false;
        }
        let idx = (self.head + self.len) % MAX_PROCESSES;
        self.buffer[idx] = Some(pid);
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<Pid> {
        if self.len == 0 {
            return None;
        }
        let pid = self.buffer[self.head].take();
        self.head = (self.head + 1) % MAX_PROCESSES;
        self.len -= 1;
        pid
    }
}

struct Inner {
    processes: [Option<Box<Process>>; MAX_PROCESSES],
    ready: ReadyQueue,
    current: Option<Pid>,
    next_pid: u64,
}

impl Inner {
    const fn new() -> Self {
        const NONE: Option<Box<Process>> = None;
        Self {
            processes: [NONE; MAX_PROCESSES],
            ready: ReadyQueue::new(),
            current: None,
            next_pid: 0,
        }
    }
}

static SCHEDULER: SpinLock<Inner> = SpinLock::new(Inner::new());

/// Reserves the next `Pid`. Normal-code-only.
pub fn allocate_pid() -> Pid {
    let mut sched = SCHEDULER.lock();
    let pid = Pid(sched.next_pid);
    sched.next_pid += 1;
    pid
}

/// Registers a fully constructed process and marks it ready to run.
/// Normal-code-only — this is the one place the scheduler's state
/// allocates (`Box::new`), which is why it must never be called from
/// interrupt context.
pub fn spawn(process: Process) {
    let pid = process.pid;
    let index = pid.0 as usize;
    let mut sched = SCHEDULER.lock();
    assert!(index < MAX_PROCESSES, "process table exhausted");
    sched.processes[index] = Some(Box::new(process));
    sched.ready.push(pid);
}

/// Marks `pid` current, activates its address space, and points TSS.RSP0
/// at its kernel stack. Returns a pointer to its trap frame, plus the
/// `rax`/`rbx` it holds (used only for the diagnostic print below —
/// reading them here avoids the caller needing to re-lock `SCHEDULER`
/// after this drops its guard).
fn switch_to(sched: &mut Inner, pid: Pid) -> (*mut TrapFrame, u64, u64) {
    sched.current = Some(pid);
    let process = sched.processes[pid.0 as usize]
        .as_mut()
        .expect("switch_to named a process that does not exist");
    process.state = ProcessState::Running;

    let kernel_stack_top: VirtAddr = process.kernel_stack_top();
    unsafe {
        process.address_space.activate();
        gdt::set_kernel_stack(kernel_stack_top);
    }

    (
        &mut process.trap_frame as *mut TrapFrame,
        process.trap_frame.rax,
        process.trap_frame.rbx,
    )
}

/// Diagnostic only, gated to roughly twice a second: shows which process
/// the round-robin queue picked and its register state at that point, so
/// alternation (and each process's counter actually changing between
/// picks, not just staying at its initial value) is directly observable
/// rather than merely "didn't crash."
fn maybe_print_switch(pid: Pid, rax: u64, rbx: u64) {
    use core::sync::atomic::{AtomicU64, Ordering};
    static LAST_PRINT_TICK: AtomicU64 = AtomicU64::new(0);
    // 51, not 50: switches happen every single tick (nothing yields
    // voluntarily yet), so an even sampling interval would always land
    // on the same parity and always show the same pid, looking like no
    // alternation is happening at all.
    let now = crate::arch::x86_64::interrupts::ticks();
    if now >= LAST_PRINT_TICK.load(Ordering::Relaxed) + 51 {
        LAST_PRINT_TICK.store(now, Ordering::Relaxed);
        crate::earlyprintln!("[sched] switched to pid {} (rax={rax}, rbx={rbx})", pid.0);
    }
}

/// Called only from the timer entry stub's ring-3 path. `current_frame`
/// points at the just-interrupted process's saved registers, on that
/// process's own kernel stack. Returns where to resume — the same
/// process if nothing else is ready, otherwise whichever process the
/// round-robin queue hands back next.
pub fn on_timer_tick(current_frame: *mut TrapFrame) -> *mut TrapFrame {
    let mut sched = SCHEDULER.lock();

    if let Some(current_pid) = sched.current {
        let index = current_pid.0 as usize;
        if let Some(process) = sched.processes[index].as_mut() {
            // SAFETY: `current_frame` is a valid, fully-initialized
            // TrapFrame — it was just captured by the entry stub.
            process.trap_frame = unsafe { *current_frame };
            process.state = ProcessState::Ready;
        }
        sched.ready.push(current_pid);
    }

    let Some(next_pid) = sched.ready.pop() else {
        // Nothing schedulable (no processes exist yet, or only the one
        // just interrupted, which was already re-queued above and will
        // simply be popped back out — either way there is nothing to
        // switch to right now).
        return current_frame;
    };

    let (frame_ptr, rax, rbx) = switch_to(&mut sched, next_pid);
    drop(sched);
    maybe_print_switch(next_pid, rax, rbx);
    frame_ptr
}

/// Starts running processes: picks the first ready one and resumes it.
/// Called exactly once, from boot code, after at least one process has
/// been [`spawn`]ed — every later switch happens automatically via
/// [`on_timer_tick`] instead. Never returns: like any other resume, this
/// only comes back to ring 0 via a future interrupt, not a normal
/// function return.
pub fn start() -> ! {
    let (first, frame_ptr, rax, rbx) = {
        let mut sched = SCHEDULER.lock();
        let first = sched
            .ready
            .pop()
            .expect("scheduler::start() called with no processes spawned");
        let (frame_ptr, rax, rbx) = switch_to(&mut sched, first);
        (first, frame_ptr, rax, rbx)
    };
    maybe_print_switch(first, rax, rbx);
    unsafe { crate::arch::x86_64::context_switch::resume(&*frame_ptr) }
}
