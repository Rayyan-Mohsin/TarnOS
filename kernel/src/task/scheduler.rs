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

/// Also the bound on how many processes can simultaneously be queued as
/// waiters on a single `ipc::Endpoint` (see `ipc::endpoint`'s `Slot`) —
/// there can never be more blocked senders or receivers on one endpoint
/// than there are processes in existence at all.
pub const MAX_PROCESSES: usize = 16;

/// See `tarnos_kcore::RingBuffer`'s doc comment — the extracted,
/// unit-tested version of what used to be a hand-copied ring buffer
/// here (and, until the same extraction, an almost-identical copy in
/// `task::executor`).
type ReadyQueue = tarnos_kcore::RingBuffer<Pid, MAX_PROCESSES>;

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
    // SYSCALL (unlike an interrupt) never switches stacks on its own, so
    // the syscall entry trampoline needs its own record of "the current
    // kernel stack," read directly rather than via the TSS.
    crate::arch::x86_64::syscall::set_syscall_kernel_stack(kernel_stack_top);

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

    let result_frame = match sched.ready.pop() {
        Some(next_pid) => {
            let (frame_ptr, rax, rbx) = switch_to(&mut sched, next_pid);
            drop(sched);
            maybe_print_switch(next_pid, rax, rbx);
            frame_ptr
        }
        None => {
            // Nothing schedulable (no processes exist yet, or only the
            // one just interrupted, which was already re-queued above
            // and will simply be popped back out) — resume exactly what
            // was interrupted.
            drop(sched);
            current_frame
        }
    };

    // Kernel tasks (the UART echo task, the console server) need to keep
    // making progress even while real processes are running and the
    // kernel's own idle loop — where they'd otherwise only ever be
    // polled — never runs again once `scheduler::start()` hands the
    // machine to them. Every timer tick is a convenient, already-
    // interrupts-disabled point to give them one, on top of whatever
    // process-switching just happened above.
    crate::task::executor::run_ready_tasks();

    result_frame
}

/// Called from the `SYS_YIELD` syscall path. Voluntarily giving up the
/// timeslice is, from the scheduler's point of view, exactly the same
/// event as the timer forcing a preemption — so this simply *is*
/// [`on_timer_tick`], reused rather than duplicated.
pub fn on_syscall_yield(current_frame: *mut TrapFrame) -> *mut TrapFrame {
    on_timer_tick(current_frame)
}

/// Picks the next ready process and switches to it, given `sched`'s lock
/// already held. If none is ready yet, gives kernel tasks one drain —
/// not zero, and not an unbounded retry loop — before finally halting:
/// a task's poll (e.g. the console server receiving a message) can
/// itself complete a rendezvous with a process that was `Blocked` on
/// the other side (see `ipc::endpoint::Endpoint::wake_receiver`), which
/// pushes that process into `ready` as a direct side effect of the
/// drain, so a process can genuinely become schedulable only *because*
/// of that one drain. Nothing en route to this function's two callers
/// (a process terminating or blocking) allocates or holds another lock
/// across the drain, so one pass is enough to observe everything it
/// could produce — a second pass would only ever find the same, already
/// re-checked, empty queue.
fn switch_to_next_or_halt(
    mut sched: crate::sync::SpinLockGuard<'_, Inner>,
    halt_message: &str,
) -> *mut TrapFrame {
    if let Some(next_pid) = sched.ready.pop() {
        return finish_switch(sched, next_pid);
    }

    drop(sched);
    crate::task::executor::run_ready_tasks();
    sched = SCHEDULER.lock();
    if let Some(next_pid) = sched.ready.pop() {
        return finish_switch(sched, next_pid);
    }

    drop(sched);
    crate::earlyprintln!("{halt_message}");
    loop {
        unsafe {
            core::arch::asm!("cli", "hlt", options(nomem, nostack));
        }
    }
}

fn finish_switch(mut sched: crate::sync::SpinLockGuard<'_, Inner>, next_pid: Pid) -> *mut TrapFrame {
    let (frame_ptr, rax, rbx) = switch_to(&mut sched, next_pid);
    drop(sched);
    maybe_print_switch(next_pid, rax, rbx);
    crate::task::executor::run_ready_tasks();
    frame_ptr
}

/// Drops the calling process (no frame to preserve — it isn't coming
/// back) and switches to whichever process is next ready. If none are,
/// this milestone has no idle process to fall back to, so it halts the
/// core here rather than returning into the caller's now-invalid stack.
///
/// Shared by two callers that are, from the scheduler's point of view,
/// exactly the same event: a process voluntarily exiting (`SYS_EXIT`,
/// via [`on_syscall_exit`]) and a process being forcibly killed after a
/// CPU exception it caused (see `arch::x86_64::idt`'s process-facing
/// fault handlers) — neither has a frame worth preserving, and both
/// need the same "drop it, run whatever's next" handling.
pub fn terminate_current_process() -> *mut TrapFrame {
    let mut sched = SCHEDULER.lock();
    if let Some(current_pid) = sched.current.take() {
        sched.processes[current_pid.0 as usize] = None;
    }
    switch_to_next_or_halt(sched, "[sched] last process exited, halting.")
}

/// Called from the `SYS_EXIT` syscall path: exiting voluntarily is
/// exactly [`terminate_current_process`]'s event, just reached from a
/// different entry stub.
pub fn on_syscall_exit(_current_frame: *mut TrapFrame) -> *mut TrapFrame {
    terminate_current_process()
}

/// Runs `f` against the currently-running process, e.g. to resolve a
/// capability index during a syscall. `None` if there is no current
/// process (should not happen when called from the syscall path, which
/// only runs while some process is executing).
pub fn with_current_process<R>(f: impl FnOnce(&mut Process) -> R) -> Option<R> {
    let mut sched = SCHEDULER.lock();
    let pid = sched.current?;
    let process = sched.processes[pid.0 as usize].as_mut()?;
    Some(f(process))
}

/// What to write into a blocked process's saved registers before waking
/// it — the two ways a `SYS_SEND`/`SYS_RECV` that had to block can later
/// complete. Built by whichever `ipc::Endpoint` operation displaces a
/// `Waiter::Process`, since only it knows which of the two just happened
/// and (for a receiver) what was actually delivered.
pub enum WakeResult {
    /// A blocked sender's message was just taken by a receiver.
    SendCompleted,
    /// A blocked receiver was just handed a message.
    RecvCompleted { tag: u64, words: [u64; 3] },
}

/// Moves a `Blocked` process back to `Ready` and into the ready queue,
/// having already written the syscall return value its blocked
/// `SYS_SEND`/`SYS_RECV` should see once resumed. Called by
/// `ipc::Endpoint` when a send or receive completes a rendezvous with a
/// process that was blocked on the other side — `Endpoint` itself has no
/// notion of a scheduler, so it hands the bookkeeping here instead of
/// doing it directly. A no-op if `pid` no longer exists (e.g. it was
/// killed by a fault while blocked — see `arch::x86_64::idt`).
pub fn wake_blocked_process(pid: Pid, result: WakeResult) {
    let mut sched = SCHEDULER.lock();
    let Some(process) = sched.processes[pid.0 as usize].as_mut() else {
        return;
    };
    match result {
        WakeResult::SendCompleted => {
            process.trap_frame.rax = 0;
        }
        WakeResult::RecvCompleted { tag, words } => {
            process.trap_frame.rax = 0;
            process.trap_frame.rdi = tag;
            process.trap_frame.rsi = words[0];
            process.trap_frame.rdx = words[1];
            process.trap_frame.r10 = words[2];
        }
    }
    process.state = ProcessState::Ready;
    sched.ready.push(pid);
}

/// Suspends the calling process inside a blocking `SYS_SEND`/`SYS_RECV`
/// that found no partner ready: persists `current_frame` into the
/// process's own `trap_frame` (so `wake_blocked_process` can resume it
/// later, exactly like a preempted process's frame is persisted in
/// `on_timer_tick`), marks it `Blocked`, and — the one thing that
/// actually distinguishes this from an ordinary preemption — does *not*
/// push it back into the ready queue, so it can never be scheduled again
/// until something wakes it. Switches to whatever's next ready exactly
/// like `terminate_current_process`, including the same "nothing left
/// to run" halt if every other process is also blocked or gone.
pub fn block_current_process(current_frame: *mut TrapFrame) -> *mut TrapFrame {
    let mut sched = SCHEDULER.lock();
    if let Some(current_pid) = sched.current {
        if let Some(process) = sched.processes[current_pid.0 as usize].as_mut() {
            // SAFETY: `current_frame` is a valid, fully-initialized
            // TrapFrame -- it's the same frame the syscall entry
            // trampoline built for this process's own trap.
            process.trap_frame = unsafe { *current_frame };
            process.state = ProcessState::Blocked;
        }
    }
    switch_to_next_or_halt(sched, "[sched] every process blocked or exited, halting.")
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
