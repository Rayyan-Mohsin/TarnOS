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
//! from normal, non-interrupt code. Process *termination* similarly
//! allocates a small, bounded `Vec` (see [`terminate_slot`]) for the
//! same reason — never from interrupt context.
use alloc::boxed::Box;
use alloc::vec::Vec;

use tarnos_abi::ExitStatus;
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

/// One process-table slot's state.
///
/// `Zombie` exists only for the window between a process exiting and
/// its recorded `parent` reaping it via `SYS_WAIT` — a bounded,
/// single-slot cost (it can sit there forever if never reaped, but
/// never grows), not the unbounded leak `Empty`-with-no-tracking would
/// otherwise reintroduce. `parent` here is never optional: a slot only
/// ever becomes a `Zombie` when someone could legitimately reap it — a
/// process with no parent goes straight to `Empty` on exit instead. See
/// `docs/adr/0007-process-lifecycle-and-termination.md`.
enum Slot {
    Empty,
    Occupied(Box<Process>),
    Zombie { parent: Pid, status: ExitStatus },
}

struct Inner {
    processes: [Slot; MAX_PROCESSES],
    /// Generation counter per table slot, independent of whatever
    /// `processes` currently holds there (so it survives
    /// `Occupied` -> `Zombie` -> `Empty` transitions without resetting)
    /// — see `Pid`'s doc comment.
    generations: [u32; MAX_PROCESSES],
    ready: ReadyQueue,
    current: Option<Pid>,
}

impl Inner {
    const fn new() -> Self {
        const EMPTY: Slot = Slot::Empty;
        Self {
            processes: [EMPTY; MAX_PROCESSES],
            generations: [0; MAX_PROCESSES],
            ready: ReadyQueue::new(),
            current: None,
        }
    }
}

static SCHEDULER: SpinLock<Inner> = SpinLock::new(Inner::new());

/// Reserves the next `Pid`: the index of the lowest currently-empty slot
/// in the process table, so a terminated process's slot is available for
/// reuse rather than the table filling up after `MAX_PROCESSES`
/// processes have ever existed, cumulatively, over the kernel's whole
/// lifetime. Returns an out-of-range `Pid` if every slot is occupied, so
/// the existing bounds check in [`spawn`]/[`spawn_suspended`] rejects it
/// with `ResourceExhausted` the same way an in-range but already-taken
/// index never could.
///
/// Bumps that slot's generation counter before handing back its `Pid` —
/// see `Pid`'s doc comment for why a stale reference to whatever
/// *previously* occupied this slot can never alias the new occupant.
///
/// Safe to call without reserving the slot atomically against a second
/// `allocate_pid()` racing in before the first's matching `spawn`/
/// `spawn_suspended` runs: every caller (trusted boot code, and
/// `SYS_SPAWN`'s handler, which runs with interrupts disabled for its
/// entire duration — see `arch::x86_64::syscall`) allocates and spawns
/// in the same straight-line sequence with nothing else able to run in
/// between on this single core.
pub fn allocate_pid() -> Pid {
    let mut sched = SCHEDULER.lock();
    let index = sched
        .processes
        .iter()
        .position(|slot| matches!(slot, Slot::Empty))
        .unwrap_or(MAX_PROCESSES);
    if index >= MAX_PROCESSES {
        return Pid::new(MAX_PROCESSES, 0);
    }
    sched.generations[index] = sched.generations[index].wrapping_add(1);
    Pid::new(index, sched.generations[index])
}

/// Registers a fully constructed process and marks it ready to run.
/// Normal-code-only — this is the one place the scheduler's state
/// allocates (`Box::new`), which is why it must never be called from
/// interrupt context.
///
/// Returns `Err(ResourceExhausted)` instead of panicking if the process
/// table is already full — reachable today via `SYS_SPAWN` once every
/// process slot is occupied.
pub fn spawn(process: Process) -> Result<(), tarnos_abi::SyscallError> {
    let pid = process.pid;
    let index = pid.index();
    if index >= MAX_PROCESSES {
        return Err(tarnos_abi::SyscallError::ResourceExhausted);
    }
    let mut sched = SCHEDULER.lock();
    sched.processes[index] = Slot::Occupied(Box::new(process));
    sched.ready.push(pid);
    Ok(())
}

/// Registers a fully constructed process **without** making it
/// schedulable — used by `SYS_SPAWN`, which must create a child in
/// [`ProcessState::Suspended`](super::process::ProcessState::Suspended)
/// so its parent can grant it capabilities before anything runs it. The
/// process occupies its process-table slot immediately (so a second
/// `allocate_pid()` can't be handed the same index), it's just absent
/// from the ready queue until [`start_child`] releases it.
pub fn spawn_suspended(process: Process) -> Result<(), tarnos_abi::SyscallError> {
    let pid = process.pid;
    let index = pid.index();
    if index >= MAX_PROCESSES {
        return Err(tarnos_abi::SyscallError::ResourceExhausted);
    }
    let mut sched = SCHEDULER.lock();
    sched.processes[index] = Slot::Occupied(Box::new(process));
    Ok(())
}

/// Looks up `pid`'s process only if it's both in range and still the
/// generation the table currently holds at that index — the combined
/// check every caller that might be handed a stale or
/// attacker-controlled `Pid` needs (see `Pid`'s doc comment).
fn occupied_mut(sched: &mut Inner, pid: Pid) -> Option<&mut Process> {
    let index = pid.index();
    if index >= MAX_PROCESSES || sched.generations[index] != pid.generation() {
        return None;
    }
    match &mut sched.processes[index] {
        Slot::Occupied(process) => Some(process),
        _ => None,
    }
}

/// Releases a `Suspended` child into the ready queue — `SYS_PROCESS_START`'s
/// implementation. Permitted only when `target` is currently `Suspended`
/// *and* its recorded `parent` is `caller`: the same structural check
/// [`with_process`] backs `SYS_GRANT` with, checked and mutated under one
/// lock acquisition so there's no window between "checked" and "acted on."
/// Once released, `target.parent` still names the creator for the
/// record — `SYS_WAIT`/`SYS_KILL` still treat that as authority for the
/// rest of the child's life (see `docs/adr/0007`), but no further
/// *grant/start*-shaped syscall does.
pub fn start_child(target: Pid, caller: Pid) -> Result<(), tarnos_abi::SyscallError> {
    let mut sched = SCHEDULER.lock();
    let Some(process) = occupied_mut(&mut sched, target) else {
        return Err(tarnos_abi::SyscallError::InvalidTarget);
    };
    if process.parent != Some(caller) || process.state != ProcessState::Suspended {
        return Err(tarnos_abi::SyscallError::InvalidTarget);
    }
    process.state = ProcessState::Ready;
    sched.ready.push(target);
    Ok(())
}

/// Marks `pid` current, activates its address space, and points TSS.RSP0
/// at its kernel stack. Returns a pointer to its trap frame, plus the
/// `rax`/`rbx` it holds (used only for the diagnostic print below —
/// reading them here avoids the caller needing to re-lock `SCHEDULER`
/// after this drops its guard).
fn switch_to(sched: &mut Inner, pid: Pid) -> (*mut TrapFrame, u64, u64) {
    sched.current = Some(pid);
    let process = match &mut sched.processes[pid.index()] {
        Slot::Occupied(process) => process,
        _ => panic!("switch_to named a process that does not exist"),
    };
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
/// rather than merely "didn't crash." Prints `pid.index()`, not the raw
/// packed value, since the generation half is an internal safety detail
/// no diagnostic reader needs to see.
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
        crate::earlyprintln!(
            "[sched] switched to pid {} (rax={rax}, rbx={rbx})",
            pid.index()
        );
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
        if let Slot::Occupied(process) = &mut sched.processes[current_pid.index()] {
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
/// of that one drain. Nothing en route to this function's callers
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
    crate::earlyprintln!(
        "[memtest] free_frames={}",
        crate::memory::phys::free_frame_count()
    );
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

/// Rebuilds the ready queue with `target` removed, if it was present.
/// `tarnos_kcore::RingBuffer` has no remove-by-value, so this drains and
/// re-pushes everything else — bounded to `MAX_PROCESSES` pop/push
/// cycles, no allocation, and only ever called once per termination, not
/// from a hot path like [`on_timer_tick`]. Safe to call even when
/// `target` was never in the queue at all (self-exit/fault-kill's own
/// pid never is) — the rebuild is then just a no-op copy.
fn remove_from_ready_queue(sched: &mut Inner, target: Pid) {
    let mut kept = ReadyQueue::new();
    while let Some(pid) = sched.ready.pop() {
        if pid != target {
            let _ = kept.push(pid);
        }
    }
    sched.ready = kept;
}

/// Extracts the process at `pid` (a no-op if its slot isn't currently
/// `Occupied` — defensive, shouldn't happen for any real caller), and
/// finalizes everything its termination cascades into:
/// - removes it from the ready queue if it was there;
/// - clears `sched.current` if it was the running process;
/// - if another process is already blocked in `SYS_WAIT` for this one
///   specifically (`process.wait_waiter`), resolves it immediately with
///   `status` — no `Zombie` needed, that's the one party who could ever
///   have claimed it;
/// - otherwise, if it has a parent that *could* someday `SYS_WAIT` it,
///   turns its slot into `Slot::Zombie`; if it has no parent, nobody
///   ever could, so the slot goes straight to `Empty`;
/// - sweeps its own leftover children the same way (see
///   [`reap_children_of`]).
///
/// The extracted `Box<Process>` (and any orphaned children's) is pushed
/// onto `pending_drops` rather than dropped here — see [`terminate_slot`]
/// for why that has to happen after the scheduler lock is released.
fn take_and_finalize_slot(
    sched: &mut Inner,
    pid: Pid,
    status: ExitStatus,
    pending_drops: &mut Vec<Box<Process>>,
) {
    let index = pid.index();
    let process = match core::mem::replace(&mut sched.processes[index], Slot::Empty) {
        Slot::Occupied(process) => process,
        other => {
            sched.processes[index] = other;
            return;
        }
    };

    remove_from_ready_queue(sched, pid);
    if sched.current == Some(pid) {
        sched.current = None;
    }

    if let Some(waiter) = process.wait_waiter {
        wake_blocked_process_locked(sched, waiter, WakeResult::WaitCompleted(status));
    } else if let Some(parent) = process.parent {
        sched.processes[index] = Slot::Zombie { parent, status };
    }
    // else: no parent and nobody waiting — the slot stays `Empty`
    // (already set by the `mem::replace` above).

    reap_children_of(sched, pid, pending_drops);
    pending_drops.push(process);
}

/// Sweeps the table once for children `exiting_pid` leaves behind:
/// - an orphaned `Suspended` child (never released via
///   `SYS_PROCESS_START`, so nothing but its now-gone parent could ever
///   have released it) is killed outright, recursively via
///   [`take_and_finalize_slot`] — closing the gap
///   `docs/adr/0006-dynamic-process-creation-and-capability-transfer.md`
///   named, now that a teardown primitive exists to close it with;
/// - an orphaned `Zombie` child (its recorded parent — the one exiting
///   right now — is the only process that could ever have `SYS_WAIT`ed
///   it) is reaped straight to `Empty`, since leaving it would make its
///   slot a *permanent* leak, worse than the bounded one a still-live
///   parent leaves;
/// - an ordinary running/blocked child is left alone — same as a real
///   OS not killing a process just because its parent died.
fn reap_children_of(sched: &mut Inner, exiting_pid: Pid, pending_drops: &mut Vec<Box<Process>>) {
    for index in 0..MAX_PROCESSES {
        let is_orphaned_suspended = matches!(
            &sched.processes[index],
            Slot::Occupied(process)
                if process.parent == Some(exiting_pid) && process.state == ProcessState::Suspended
        );
        if is_orphaned_suspended {
            let child_pid = Pid::new(index, sched.generations[index]);
            take_and_finalize_slot(sched, child_pid, ExitStatus::Killed, pending_drops);
            continue;
        }
        if let Slot::Zombie { parent, .. } = &sched.processes[index] {
            if *parent == exiting_pid {
                sched.processes[index] = Slot::Empty;
            }
        }
    }
}

/// Shared core of self-exit, fault-kill, and `SYS_KILL`: finalizes the
/// process named by `pid` and everything that cascades from it (see
/// [`take_and_finalize_slot`]), then drops its `Box<Process>` — freeing
/// its `AddressSpace`'s physical frames via `Drop for AddressSpace`
/// (`memory::virt`) — only *after* releasing the scheduler lock, since
/// that walk is a variable-length operation this codebase's convention
/// keeps off the lock's critical path (see `resolve_endpoint`'s and
/// `sys_grant`'s doc comments for the same rule applied elsewhere).
fn terminate_slot(pid: Pid, status: ExitStatus) {
    let mut pending_drops = Vec::new();
    {
        let mut sched = SCHEDULER.lock();
        take_and_finalize_slot(&mut sched, pid, status, &mut pending_drops);
    }
    drop(pending_drops); // outside the lock: frees every AddressSpace's frames
}

/// Drops the calling process (no frame to preserve — it isn't coming
/// back) and switches to whichever process is next ready. If none are,
/// this milestone has no idle process to fall back to, so it halts the
/// core here rather than returning into the caller's now-invalid stack.
///
/// Shared by two callers that are, from the scheduler's point of view,
/// exactly the same event modulo `status`: a process voluntarily exiting
/// (`SYS_EXIT`, via [`on_syscall_exit`], `status = Exited(code)`) and a
/// process being forcibly killed after a CPU exception it caused (see
/// `arch::x86_64::idt`'s process-facing fault handlers, `status =
/// Faulted`) — neither has a frame worth preserving, and both need the
/// same "finalize it, run whatever's next" handling.
pub fn terminate_current_process(status: ExitStatus) -> *mut TrapFrame {
    let current = {
        let sched = SCHEDULER.lock();
        sched.current
    };
    if let Some(pid) = current {
        terminate_slot(pid, status);
    }
    let sched = SCHEDULER.lock();
    switch_to_next_or_halt(sched, "[sched] last process exited, halting.")
}

/// Called from the `SYS_EXIT` syscall path: exiting voluntarily is
/// exactly [`terminate_current_process`]'s event, just reached from a
/// different entry stub. Reads the exit code the process placed in
/// `rdi` — previously computed by userland and then silently dropped by
/// the kernel; see `docs/adr/0007`.
pub fn on_syscall_exit(current_frame: *mut TrapFrame) -> *mut TrapFrame {
    // SAFETY: `current_frame` is the same valid, fully-initialized
    // TrapFrame the syscall entry trampoline built for this trap.
    let code = unsafe { (*current_frame).rdi } as i32;
    terminate_current_process(ExitStatus::Exited(code))
}

/// `SYS_KILL`'s implementation: immediately terminates `target`,
/// regardless of its current state (`Ready`, `Blocked`, or `Suspended`
/// — not restricted to `Suspended` the way `SYS_GRANT`/`SYS_PROCESS_START`
/// are), as long as it's a live child of `caller`. Rejects a target that
/// isn't a live child of the caller at all, or one that's already a
/// `Zombie` — reaping one of those is `SYS_WAIT`'s job, not this one's.
///
/// Since nothing is its own parent, and there's exactly one running
/// process on this single core, `target` can never be `caller` itself —
/// this never needs to trigger a scheduler switch, matching
/// `sys_grant`/`sys_process_start`'s shape.
pub fn terminate_process(target: Pid, caller: Pid) -> Result<(), tarnos_abi::SyscallError> {
    {
        let mut sched = SCHEDULER.lock();
        let Some(process) = occupied_mut(&mut sched, target) else {
            return Err(tarnos_abi::SyscallError::InvalidTarget);
        };
        if process.parent != Some(caller) {
            return Err(tarnos_abi::SyscallError::InvalidTarget);
        }
    }
    terminate_slot(target, ExitStatus::Killed);
    Ok(())
}

/// `SYS_WAIT`'s implementation. If `target` (a child of `caller`) has
/// already exited (`Slot::Zombie`), reaps it immediately and returns its
/// status (`Ok(Some(status))`). If `target` is still alive, records
/// `caller` as its `wait_waiter` — so [`take_and_finalize_slot`] can
/// resolve it directly once `target` actually terminates, whatever the
/// cause — and returns `Ok(None)`, telling the caller (`sys_wait`'s
/// syscall handler) to block via [`block_current_process`], exactly
/// like a blocking `sys_send`/`sys_recv`. Since only `target.parent` is
/// ever permitted to become its `wait_waiter`, at most one process can
/// ever legitimately be waiting on a given target — no wait *queue* is
/// needed, just this one field.
pub fn wait_for_child(
    target: Pid,
    caller: Pid,
) -> Result<Option<ExitStatus>, tarnos_abi::SyscallError> {
    let mut sched = SCHEDULER.lock();
    let index = target.index();
    if index >= MAX_PROCESSES || sched.generations[index] != target.generation() {
        return Err(tarnos_abi::SyscallError::InvalidTarget);
    }
    match &mut sched.processes[index] {
        Slot::Zombie { parent, status } if *parent == caller => {
            let status = *status;
            sched.processes[index] = Slot::Empty;
            Ok(Some(status))
        }
        Slot::Occupied(process) if process.parent == Some(caller) => {
            process.wait_waiter = Some(caller);
            Ok(None)
        }
        _ => Err(tarnos_abi::SyscallError::InvalidTarget),
    }
}

/// Runs `f` against whichever process `pid` names, e.g. to check or
/// mutate a specific process's capability table during a syscall like
/// `SYS_GRANT`. `None` if `pid` doesn't currently name a live process —
/// `pid` may be attacker-controlled input from a syscall register (an
/// arbitrary `u64`), so this looks up via [`occupied_mut`] (bounds- and
/// generation-checked) rather than direct indexing.
pub fn with_process<R>(pid: Pid, f: impl FnOnce(&mut Process) -> R) -> Option<R> {
    let mut sched = SCHEDULER.lock();
    let process = occupied_mut(&mut sched, pid)?;
    Some(f(process))
}

/// Runs `f` against the currently-running process, e.g. to resolve a
/// capability index during a syscall. `None` if there is no current
/// process (should not happen when called from the syscall path, which
/// only runs while some process is executing).
pub fn with_current_process<R>(f: impl FnOnce(&mut Process) -> R) -> Option<R> {
    let pid = {
        let sched = SCHEDULER.lock();
        sched.current?
    };
    with_process(pid, f)
}

/// What to write into a blocked process's saved registers before waking
/// it. Built by whichever operation displaces the corresponding wait —
/// `ipc::Endpoint` for a `Waiter::Process` (send/recv), or
/// [`take_and_finalize_slot`] for a `SYS_WAIT` waiter — since only the
/// caller knows which of these just happened and (for a receiver, or a
/// wait) what was actually delivered.
pub enum WakeResult {
    /// A blocked sender's message was just taken by a receiver.
    SendCompleted,
    /// A blocked receiver was just handed a message.
    RecvCompleted { tag: u64, words: [u64; 3] },
    /// A blocked `SYS_WAIT` caller's target just terminated.
    WaitCompleted(ExitStatus),
}

fn wake_blocked_process_locked(sched: &mut Inner, pid: Pid, result: WakeResult) {
    let Some(process) = occupied_mut(sched, pid) else {
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
        WakeResult::WaitCompleted(status) => {
            let (kind, code) = status.to_regs();
            process.trap_frame.rax = 0;
            process.trap_frame.rdi = kind;
            process.trap_frame.rsi = code;
        }
    }
    process.state = ProcessState::Ready;
    sched.ready.push(pid);
}

/// Moves a `Blocked` process back to `Ready` and into the ready queue,
/// having already written the syscall return value its blocked call
/// should see once resumed. Called by `ipc::Endpoint` when a send or
/// receive completes a rendezvous with a process that was blocked on
/// the other side — `Endpoint` itself has no notion of a scheduler, so
/// it hands the bookkeeping here instead of doing it directly. A no-op
/// if `pid` no longer names the same process (e.g. it was killed while
/// blocked — see `arch::x86_64::idt`/`SYS_KILL` — or its slot has since
/// been reused by an unrelated process; see `Pid`'s doc comment).
pub fn wake_blocked_process(pid: Pid, result: WakeResult) {
    let mut sched = SCHEDULER.lock();
    wake_blocked_process_locked(&mut sched, pid, result);
}

/// Suspends the calling process inside a blocking `SYS_SEND`/`SYS_RECV`/
/// `SYS_WAIT` that found no partner ready: persists `current_frame` into
/// the process's own `trap_frame` (so `wake_blocked_process` can resume
/// it later, exactly like a preempted process's frame is persisted in
/// `on_timer_tick`), marks it `Blocked`, and — the one thing that
/// actually distinguishes this from an ordinary preemption — does *not*
/// push it back into the ready queue, so it can never be scheduled again
/// until something wakes it. Switches to whatever's next ready exactly
/// like `terminate_current_process`, including the same "nothing left
/// to run" halt if every other process is also blocked or gone.
pub fn block_current_process(current_frame: *mut TrapFrame) -> *mut TrapFrame {
    let mut sched = SCHEDULER.lock();
    if let Some(current_pid) = sched.current {
        if let Slot::Occupied(process) = &mut sched.processes[current_pid.index()] {
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
