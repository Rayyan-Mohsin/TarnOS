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
use core::sync::atomic::Ordering;

use spin::Once;
use tarnos_abi::ExitStatus;
use x86_64::VirtAddr;

use crate::arch::x86_64::context_switch::TrapFrame;
use crate::arch::x86_64::percpu::{self, MAX_CORES};
use crate::arch::x86_64::{gdt, lapic};
use crate::memory::virt::AddressSpace;
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
///
/// `Reserved` exists only for the window between [`allocate_pid`]
/// claiming an index and the matching [`spawn`]/[`spawn_suspended`]
/// actually filling it with a constructed `Process` — see
/// `allocate_pid`'s doc comment for the cross-core race this closes.
enum Slot {
    Empty,
    Reserved,
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
    /// The process currently running *on each core* — `current[i]` is
    /// core `i`'s own, indexed by `percpu::core_index()`. A single
    /// scalar sufficed before this milestone (there was only ever one
    /// core); see `docs/adr/0010-cross-core-scheduling.md`. Every
    /// existing call site that reads/writes "the current process"
    /// already only ever concerns the calling core's own entry, so
    /// indexing by `percpu::core_index()` is the whole change — except
    /// [`take_and_finalize_slot`], which must clear *whichever* core (if
    /// any) still shows a terminating `pid` as current, since a
    /// cross-core `SYS_KILL` can terminate a process another core is
    /// running right now.
    current: [Option<Pid>; MAX_CORES],
}

impl Inner {
    const fn new() -> Self {
        const EMPTY: Slot = Slot::Empty;
        Self {
            processes: [EMPTY; MAX_PROCESSES],
            generations: [0; MAX_PROCESSES],
            ready: ReadyQueue::new(),
            current: [None; MAX_CORES],
        }
    }
}

/// Sets `core`'s current process in both `Inner.current` (authoritative,
/// `SCHEDULER`-locked) and `percpu::PerCpuSlot.current` (a lock-free
/// mirror another core can poll without contending `SCHEDULER` — see
/// `terminate_process`'s cross-core eviction wait).
fn set_current(sched: &mut Inner, core: usize, pid: Option<Pid>) {
    sched.current[core] = pid;
    percpu::slot(core)
        .current
        .store(pid.map_or(0, |p| p.0), Ordering::Release);
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
/// Immediately marks the chosen slot [`Slot::Reserved`], in the same
/// `SCHEDULER` acquisition that found it — closing a real, reproduced
/// cross-core race: an earlier version of this function only bumped the
/// generation counter and returned, leaving the slot itself `Empty`
/// until the caller's own later `spawn`/`spawn_suspended` call filled
/// it. That was safe against a *second* `allocate_pid()` call racing in
/// from the *same* core (nothing else can run there in between), but
/// not from a *different* one — and once `SYS_SPAWN` could itself be a
/// process's very first instruction after being scheduled (found while
/// prototyping a combined multi-workload stress scenario, deferred to a
/// later milestone, that spawns several concurrently-runnable processes
/// each immediately spawning a child of their own), a second core's
/// `allocate_pid()` could land in the
/// exact gap between the first core's own `allocate_pid()` and its
/// matching `spawn`, see the same slot as `Empty`, and be handed the
/// identical index with a bumped generation. Whichever side's `spawn`/
/// `spawn_suspended` ran second then silently overwrote the other's
/// `Process` in the table — while the loser's own `Pid` (a different
/// generation) legitimately kept resolving to *that* overwriting
/// process's own current generation, aliasing a totally unrelated
/// process. Observed in practice as a process running another
/// process's code from process index confusion alone, no memory
/// corruption or unsafe code involved. See `docs/adr/0011`.
///
/// A caller that fails before its matching `spawn`/`spawn_suspended`
/// ever runs (today: only `SYS_SPAWN`'s handler, if ELF loading fails)
/// must call [`release_reservation`] to give the slot back — see its
/// own doc comment.
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
    sched.processes[index] = Slot::Reserved;
    Pid::new(index, sched.generations[index])
}

/// Gives back a slot [`allocate_pid`] reserved when its caller fails
/// before ever calling the matching `spawn`/`spawn_suspended` — today,
/// only `SYS_SPAWN`'s handler on an ELF-load failure. A no-op if `pid`'s
/// generation no longer matches (defensive; shouldn't happen, since
/// nothing else can legitimately touch a still-`Reserved` slot).
pub fn release_reservation(pid: Pid) {
    let mut sched = SCHEDULER.lock();
    let index = pid.index();
    if index >= MAX_PROCESSES || sched.generations[index] != pid.generation() {
        return;
    }
    if matches!(sched.processes[index], Slot::Reserved) {
        sched.processes[index] = Slot::Empty;
    }
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
    drop(sched);
    notify_idle_cores();
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
    drop(sched);
    notify_idle_cores();
    Ok(())
}

/// Marks `pid` current, activates its address space, and points TSS.RSP0
/// at its kernel stack. Returns a pointer to its trap frame, plus the
/// `rax`/`rbx` it holds (used only for the diagnostic print below —
/// reading them here avoids the caller needing to re-lock `SCHEDULER`
/// after this drops its guard).
fn switch_to(sched: &mut Inner, pid: Pid) -> (*mut TrapFrame, u64, u64) {
    set_current(sched, percpu::core_index(), Some(pid));
    let index = pid.index();
    // Every other pid resolution in this module (`occupied_mut`,
    // `wait_for_child`, `on_reschedule_ipi`) checks the table's current
    // generation for this slot before trusting whatever's occupying it
    // — `pid` alone (an attacker- or bug-controlled value elsewhere)
    // never implies it still names the same process. `switch_to` is the
    // one exception: every caller passes a `pid` freshly popped from
    // `sched.ready`, which this module's own invariants should always
    // keep in sync with the table's actual occupants, so this should
    // never fire. Asserted anyway, defensively: silently trusting a
    // stale entry here would mean genuinely running a *different*
    // process's code and kernel stack under the wrong identity, not a
    // clean, attributable failure — exactly the shape of a real,
    // still-unresolved cross-core corruption bug under heavy
    // concurrent load (see `docs/adr/0012`, once written).
    assert_eq!(
        sched.generations[index],
        pid.generation(),
        "switch_to: {pid:?} (slot {index}) does not match the table's current generation \
         ({}) for that slot -- a stale ready-queue entry surviving a slot reuse, or a genuine \
         cross-core scheduling race",
        sched.generations[index]
    );
    let process = match &mut sched.processes[index] {
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
/// the round-robin queue picked, which core is now running it, and its
/// register state at that point, so alternation (and each process's
/// counter actually changing between picks, not just staying at its
/// initial value) is directly observable rather than merely "didn't
/// crash." The core index is what `xtask`'s cross-core scenarios grep
/// for to confirm a process genuinely ran somewhere other than the BSP,
/// not just "eventually finished." Prints `pid.index()`, not the raw
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
            "[sched] switched to pid {} on core {} (rax={rax}, rbx={rbx})",
            pid.index(),
            percpu::core_index()
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
    let core = percpu::core_index();

    if let Some(current_pid) = sched.current[core] {
        percpu::slot(core).preempt_count.fetch_add(1, Ordering::Relaxed);
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

/// True only when every process-table slot is genuinely `Empty` — no
/// process exists anywhere, on any core, in any state. Distinct from
/// "nothing is ready right now": under single-core scheduling those were
/// the same thing (nothing else could ever make something ready again),
/// but with multiple cores, a process can be `Blocked`/`Suspended`/
/// `Running` on some *other* core while this core's own queue check
/// comes up empty. Gates [`switch_to_next_or_halt`]'s permanent,
/// unwakeable halt — reserved for "the whole machine is done," not
/// merely "this core has nothing to do at this exact instant."
fn all_processes_empty(sched: &Inner) -> bool {
    sched.processes.iter().all(|slot| matches!(slot, Slot::Empty))
}

/// The genuine end state: prints `halt_message` (existing `xtask`
/// scenarios grep for this exact text, so it must only ever fire when
/// [`all_processes_empty`] is true) and parks this core forever. Never
/// returns.
fn permanent_halt(halt_message: &str) -> ! {
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

/// A single, shared, upper-half-only `AddressSpace` (no user mappings at
/// all — exactly what `AddressSpace::new()` already builds) every core
/// activates while idling with nothing to run. Without this, an idle
/// core's CR3 keeps pointing at whatever `AddressSpace` it last ran —
/// if that gets torn down and its PML4 frame recycled by a *different*
/// core while this one is still sitting on it, a stray page-table walk
/// here could read a half-rewritten table. Since nothing in this kernel
/// uses PCID or global pages, activating this (an ordinary `mov cr3`)
/// flushes this core's entire TLB, closing the gap with no IPI-based
/// shootdown protocol needed — see `docs/adr/0010`.
static IDLE_ADDRESS_SPACE: Once<AddressSpace> = Once::new();

fn build_idle_address_space() -> AddressSpace {
    AddressSpace::new().expect("out of memory building the shared idle address space")
}

/// Builds [`IDLE_ADDRESS_SPACE`] eagerly, once, during boot -- called from
/// `main.rs` right after every other eager, up-front kernel-half mapping
/// is already in place (kernel stacks, double-fault stacks, LAPIC MMIO,
/// every core's idle stack), so this address space's one-time PML4
/// snapshot (see [`AddressSpace::new`]) correctly includes all of them.
///
/// Not just an optimization: [`activate_idle_address_space`] would
/// otherwise build this the *first* time any core actually goes idle,
/// which can happen well into a test run (or real usage) -- allocating
/// its one PML4 frame at an unpredictable point looks exactly like a
/// physical-memory leak to anything that snapshots
/// `memory::phys::free_frame_count()` before that point and checks it
/// returns to the same value later (as `xtask test-process-lifecycle`
/// does; this was a real, reproduced false-positive "AddressSpace
/// teardown is leaking physical memory" failure). Building it here,
/// before any such baseline is ever taken, closes that gap -- this
/// address space is a permanent, always-referenced part of the kernel's
/// own footprint, like the GDT/IDT/kernel stacks, not something that
/// should ever appear as a delta.
pub fn init_idle_address_space() {
    IDLE_ADDRESS_SPACE.call_once(build_idle_address_space);
}

fn activate_idle_address_space() {
    let idle_as = IDLE_ADDRESS_SPACE.call_once(build_idle_address_space);
    unsafe {
        idle_as.activate();
    }
}

/// Parks this core until the shared ready queue plausibly has something
/// in it — interrupt-driven, not a busy poll. May return spuriously (a
/// stray LAPIC spurious interrupt, or another core winning the race for
/// whatever just appeared) — callers must re-check the queue themselves
/// after this returns, the same contract as any condvar-style wait.
///
/// An earlier version of this milestone used a plain busy poll here
/// instead (`spin_loop()` in a tight lock-check-unlock loop), reasoned to
/// be safe because the BSP's own periodic timer tick would eventually
/// re-check the same shared queue regardless. That reasoning was correct
/// about *starvation*, but missed the actual cost: every idle core pins
/// one full host CPU at 100%, and this milestone's own adversarial
/// testing (`xtask test-smp-kill-cross-core`'s ancestor, `test-smp-ipi`,
/// caught it first) reproduced genuine hangs and spurious faults under
/// realistic host contention (as few host cores as guest vCPUs, exactly
/// what a GitHub Actions runner or this project's own dev sandbox looks
/// like) — the active core was starved of real CPU time by three
/// spinning idle ones. Replaced with a real interrupt-driven halt.
///
/// # Lost-wakeup correctness
/// Pushing new work (`spawn`, `start_child`, `wake_blocked_process`)
/// sends a targeted `RESCHEDULE_VECTOR` IPI to every core it finds marked
/// idle (see [`notify_idle_cores`]). The classic hazard this must close:
/// if this core already checked the queue once (found it empty) and a
/// push on another core checks *this* core's idle flag before this core
/// has actually set it, that push correctly sees "not idle yet" and
/// (reasonably) sends no IPI — yet this core is about to park anyway,
/// missing that work forever. Closed by checking *twice*, with setting
/// the flag in between rather than before: the first check (before the
/// flag is visible to anyone) catches anything already queued; setting
/// `idle` makes this core discoverable to any push from that instant on;
/// the second check catches anything that landed in the gap between the
/// two. A push arriving after the second check is guaranteed to observe
/// `idle == true` and IPI this core — and the final `sti; hlt`, issued as
/// one instruction pair via a raw `asm!` block (never through
/// `SpinLockGuard`'s ordinary drop-then-separately-re-enable, which the
/// compiler is free to place other instructions inside), is the standard
/// idiom for the one gap that's still unavoidable: `sti`'s one-
/// instruction interrupt shadow guarantees `hlt` itself executes before
/// any interrupt raised in that exact instant is serviced, so a wake IPI
/// landing between "queue checked empty" and "actually halted" still
/// reaches this `hlt` instead of being silently coalesced into nothing.
fn park_until_woken(core: usize) {
    unsafe { core::arch::asm!("cli", options(nomem, nostack)) };

    if !SCHEDULER.lock().ready.is_empty() {
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
        return;
    }

    percpu::slot(core).idle.store(true, Ordering::SeqCst);
    if !SCHEDULER.lock().ready.is_empty() {
        percpu::slot(core).idle.store(false, Ordering::SeqCst);
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
        return;
    }

    unsafe { core::arch::asm!("sti", "hlt", options(nomem, nostack)) };
    percpu::slot(core).idle.store(false, Ordering::SeqCst);
}

/// Sends a targeted `RESCHEDULE_VECTOR` IPI to every core currently
/// marked idle (see [`park_until_woken`]). Called after pushing new work
/// into the shared ready queue — `spawn`, `start_child`,
/// `wake_blocked_process(_locked)` — so a core parked in `hlt` with
/// nothing to do notices promptly instead of waiting for some unrelated
/// interrupt. Deliberately notifies every idle core rather than picking
/// one: harmless (a core that loses the race for the one new item just
/// re-parks immediately), and avoids needing to reason about which single
/// core is "the right one" when several might be idle at once.
fn notify_idle_cores() {
    for core in 0..MAX_CORES {
        if percpu::slot(core).idle.load(Ordering::SeqCst) {
            lapic::send_ipi(percpu::slot(core).lapic_id(), lapic::RESCHEDULE_VECTOR);
        }
    }
}

/// The shared "wait for and run whatever the scheduler hands this core"
/// loop body. Assumes it is *already* running on a stack no live process
/// owns -- either a core's own dedicated idle stack (see
/// [`abandon_process_stack_and_idle`]), or the boot-time stack
/// `smp::ap_entry_trampoline`/`arch::x86_64::smp` hands an AP before it
/// ever runs a process at all. Never returns: each iteration either
/// resumes a process via [`context_switch::resume`] (which itself never
/// returns to this stack) or parks via [`park_until_woken`] and loops.
///
/// `halt_message` is `None` for the boot-time entry points
/// ([`ap_enter_scheduler`]/[`start`]), where finding the process table
/// empty only ever means "nothing has been spawned yet," never "nothing
/// ever will be" -- only a core that has already run at least one
/// process to completion can conclude the latter, so those two never
/// call [`permanent_halt`] at all, just keep parking forever.
///
/// [`context_switch::resume`]: crate::arch::x86_64::context_switch::resume
fn idle_loop_on_own_stack(core: usize, halt_message: Option<&'static str>) -> ! {
    loop {
        // Drained on *every* iteration, not just once before the first
        // `park_until_woken` -- a kernel task's own wake (its `Waker`
        // firing, e.g. the console server's endpoint receiving a
        // message) only ever marks it ready in `task::executor`'s own
        // queue, never touches `SCHEDULER.ready` directly. An earlier
        // version of this loop drained only when it had already found a
        // process to run, so once this core reached `park_until_woken`
        // with nothing immediately ready, a task becoming ready afterward
        // (its poll is what would actually complete the rendezvous and
        // push a process into `SCHEDULER.ready`) was never picked up --
        // this core would keep waking on unrelated interrupts (the PIT
        // tick, say) and going straight back to `park_until_woken`
        // without ever running the task that could have unblocked
        // things, hanging indefinitely. Cheap to call unconditionally:
        // draining an executor with nothing ready is a no-op, and this
        // loop only ever spins again after `park_until_woken`'s own
        // interrupt-driven wait, never busily.
        crate::task::executor::run_ready_tasks();
        let mut sched = SCHEDULER.lock();
        if let Some(next_pid) = sched.ready.pop() {
            let (frame_ptr, rax, rbx) = switch_to(&mut sched, next_pid);
            drop(sched);
            maybe_print_switch(next_pid, rax, rbx);
            unsafe { crate::arch::x86_64::context_switch::resume(&*frame_ptr) }
        }
        if let Some(msg) = halt_message {
            if all_processes_empty(&sched) {
                drop(sched);
                permanent_halt(msg);
            }
        }
        drop(sched);
        park_until_woken(core);
    }
}

/// `extern "C"` landing pad for [`abandon_process_stack_and_idle`]'s raw
/// stack switch -- reassembles the `&'static str` its two register
/// arguments were decomposed into (a fat pointer can't cross a raw
/// `asm!` call directly), releases the `SCHEDULER` lock
/// [`switch_to_next_or_halt`] deliberately left held across the switch
/// (see that function's doc comment), and hands off to
/// [`idle_loop_on_own_stack`].
///
/// # Safety
/// `msg_ptr`/`msg_len` must together describe a valid, `'static` UTF-8
/// string -- true for every real caller, which only ever passes through
/// a `&'static str` it already had. `SCHEDULER` must actually still be
/// locked by the caller that jumped here -- true for
/// [`abandon_process_stack_and_idle`]'s one call site.
extern "C" fn idle_loop_trampoline(core: u64, msg_ptr: *const u8, msg_len: u64) -> ! {
    let halt_message: &'static str =
        unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(msg_ptr, msg_len as usize)) };
    // SAFETY: see this function's own doc comment.
    unsafe { SCHEDULER.force_unlock() };
    activate_idle_address_space();
    idle_loop_on_own_stack(core as usize, Some(halt_message))
}

/// Switches this core onto its own dedicated idle stack (the same one
/// `arch::x86_64::smp` mapped for it at bring-up -- see
/// `smp::idle_stack_top_addr`) and enters [`idle_loop_on_own_stack`]
/// there. Never returns.
///
/// # Why a stack switch is required here, not just an address-space one
/// A blocked/exited/evicted process's kernel stack is fixed *per process
/// slot*, not per core (`Process::kernel_stack_top`) -- deliberately, so
/// the same stack is simply reused the next time that slot's occupant
/// (this one, or the next process to take that table index) runs,
/// regardless of which core. [`switch_to_next_or_halt`] is called while
/// still running *on that very stack* (it's whatever the syscall/
/// interrupt entry stub that led here was using). Naively looping right
/// there to wait for more work -- which an earlier version of this
/// milestone did -- leaves this core's entire C call chain (locals,
/// return addresses, this very stack-switch decision) sitting on that
/// stack for as long as this core stays idle. If the *same process*
/// then wakes up and gets picked up by *any* core (including a
/// completely different one) for its next syscall, that core's entry
/// stub starts pushing a fresh `TrapFrame` from the *same fixed*
/// `kernel_stack_top()` address downward -- directly overwriting
/// whatever this core still had live there. This was a real, reproduced
/// bug (caught by this milestone's own `xtask test-smp-ipi`, manifesting
/// as `percpu::slot()` being called with a stack address instead of a
/// core index -- a corrupted local variable, not a logic error):
/// swapping onto a stack no process owns *before* waiting closes it, the
/// same reasoning `smp::ap_entry_trampoline` already applies to a
/// freshly-booted AP that has no process stack to abandon in the first
/// place.
///
/// # `SCHEDULER` stays locked across the switch
/// The caller ([`switch_to_next_or_halt`]) must still hold `SCHEDULER`'s
/// lock and must not have dropped it -- simply never dropping the guard
/// it took is what keeps the lock held here, since a guard whose `Drop`
/// sits behind a diverging call like this one never runs at all, not
/// merely later. That's deliberate, not an oversight: the stack-sharing
/// bug above was only *mostly* fixed by swapping stacks before parking.
/// Everything this function's caller did first -- one more ready-queue
/// check, a kernel-task drain, building the idle address space -- used to
/// run first, still on the outgoing process's own stack, and a real,
/// reproduced adversarial test (`xtask test-smp-wait-cross-core`) showed
/// that window is plenty wide enough for the *same* process to be woken
/// and dispatched onto a different core in the meantime, whose next
/// syscall entry starts overwriting this exact stack while this core is
/// still using it -- the identical hazard, just with a smaller window
/// instead of none. Holding `SCHEDULER` continuously from the moment this
/// process stopped being current until this core has actually finished
/// switching away closes it completely: waking or re-dispatching that
/// process is impossible without the same lock (`wake_blocked_process`,
/// `allocate_pid`, `start_child` all take it), so nothing else can touch
/// this stack's fixed address until [`idle_loop_trampoline`] releases the
/// lock from the safe side of the switch. See
/// `docs/adr/0010-cross-core-scheduling.md`.
fn abandon_process_stack_and_idle(core: usize, halt_message: &'static str) -> ! {
    let idle_top = crate::arch::x86_64::smp::idle_stack_top_addr(core).as_u64();
    let msg_ptr = halt_message.as_ptr();
    let msg_len = halt_message.len() as u64;
    unsafe {
        core::arch::asm!(
            "mov rsp, {top}",
            "call {trampoline}",
            top = in(reg) idle_top,
            trampoline = sym idle_loop_trampoline,
            in("rdi") core as u64,
            in("rsi") msg_ptr,
            in("rdx") msg_len,
            options(noreturn),
        );
    }
}

/// Picks the next ready process and switches to it, given `sched`'s lock
/// already held. If one is immediately ready, this is the *only* branch
/// that's still safe to run on the stack this core is already on — see
/// below — so this stays a plain, non-diverging function call
/// ([`finish_switch`]) that returns normally, exactly like every syscall
/// handler's ordinary return path.
///
/// If nothing is ready yet, this deliberately does **not** drop `sched`
/// before abandoning the stack (see [`abandon_process_stack_and_idle`]) —
/// simply never dropping a `SpinLockGuard` is what keeps a lock held
/// across a diverging call, since the guard's `Drop` then sits behind
/// code that never runs, not merely behind code that runs later. Two
/// earlier versions of this function got this wrong in smaller and
/// smaller ways, both caught by the same real, reproduced adversarial
/// test (`xtask test-smp-wait-cross-core`, manifesting as a
/// general-protection fault at an unpredictable kernel address that
/// changed between runs, or an outright wrong exit status delivered to
/// the waiting parent — the signature of two cores racing on the same
/// memory, not a deterministic logic error):
///
/// - The first version dropped the lock, ran a kernel-task drain, took a
///   *second* lock acquisition to recheck the ready queue, and only
///   *then* abandoned the stack if still nothing was ready. All of that
///   ran on the stack of a process that — unlike one
///   [`terminate_current_process`] already finalized before ever calling
///   this — [`block_current_process`] leaves merely `Blocked`, still
///   fully alive and able to be woken and dispatched onto a *different*
///   core at any instant by a completely unrelated event (e.g. the
///   target it's `SYS_WAIT`ing for exiting on that other core right
///   now). From that instant on, nothing stops that other core's next
///   syscall entry for it from pushing a fresh `TrapFrame` from the top
///   of this exact same fixed per-PID kernel stack, while this core is
///   still using deeper addresses on the very same stack for its own
///   locals and call frames.
/// - The second version shrank that window (dropping straight to
///   [`abandon_process_stack_and_idle`] with no drain/recheck first) but
///   didn't close it: dropping the lock at all, even briefly, is exactly
///   the signal every other core's `wake_blocked_process`/`start_child`/
///   `allocate_pid` is waiting on before it can touch this process again.
///   Under real QEMU/TCG scheduling noise that smaller window still lost
///   the race often enough to fail about a third of the time under
///   repeated stress.
///
/// Holding `SCHEDULER` continuously from here through the actual stack
/// switch closes this completely rather than just shrinking it: every
/// operation that could make this process visible to another core again
/// (waking it, migrating it, reusing its table slot) needs this same
/// lock, so none of them can run until [`idle_loop_trampoline`] releases
/// it from the safe side of the switch, once this core is no longer
/// using the stack at all. See `docs/adr/0010-cross-core-scheduling.md`.
fn switch_to_next_or_halt(
    mut sched: crate::sync::SpinLockGuard<'_, Inner>,
    halt_message: &'static str,
) -> *mut TrapFrame {
    if let Some(next_pid) = sched.ready.pop() {
        return finish_switch(sched, next_pid);
    }

    // No `drop(sched)` here -- see this function's doc comment.
    // SAFETY: `sched` is simply never dropped from this point on (this
    // function diverges into `abandon_process_stack_and_idle`, whose own
    // doc comment documents and relies on exactly this), so `SCHEDULER`
    // stays locked until `idle_loop_trampoline` releases it.
    core::mem::forget(sched);
    abandon_process_stack_and_idle(percpu::core_index(), halt_message);
}

/// The BSP's own first entry, right after `main.rs` spawns `init`, and
/// every additional core's entry right after it finishes its own
/// bring-up (GDT/IDT/LAPIC) -- see [`idle_loop_on_own_stack`], which does
/// the actual work. Already running on a stack no process owns in both
/// cases (this core's own dedicated idle stack, set up by
/// `smp::ap_entry_trampoline`/`smp::bring_up_aps` before either ever
/// runs anything), so -- unlike [`switch_to_next_or_halt`] -- there is no
/// stack to abandon here.
pub fn ap_enter_scheduler() -> ! {
    activate_idle_address_space();
    idle_loop_on_own_stack(percpu::core_index(), None)
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
    // Normally at most one core's `current` can ever name `pid` (a process
    // only ever runs on one core at a time), but scanning all of them
    // rather than assuming "the caller's own core" is what makes this
    // correct when called from a cross-core `SYS_KILL` path too -- see
    // `terminate_process`.
    for core in 0..MAX_CORES {
        if sched.current[core] == Some(pid) {
            set_current(sched, core, None);
        }
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
/// - an ordinary `Ready`/`Running`/`Blocked` child is left running — same
///   as a real OS not killing a process just because its parent died —
///   but has its `parent` field cleared to `None`. Without this, a child
///   that happened to be merely `Ready` (already woken by some rendezvous
///   but not yet actually resumed) at the exact moment this sweep ran
///   would keep its stale `parent` pointing at a pid nothing will ever
///   reuse for its old parent again; when that child *later* exits on
///   its own, [`take_and_finalize_slot`] would see a non-`None` `parent`
///   and turn its slot into a `Zombie` nobody can ever `SYS_WAIT` (its
///   real parent is gone) — a permanent, unreapable slot leak that also
///   makes [`all_processes_empty`] never true again, hanging the whole
///   machine's terminal halt forever. A real, reproduced bug: found via
///   `xtask test-spawn-ipc` intermittently hanging instead of reaching
///   its expected clean-exit halt, whenever a timer tick happened to
///   preempt right as the parent/child rendezvous left the child exactly
///   in this window. Clearing `parent` here makes a later self-exit go
///   straight to `Empty` instead (the same path a process with no parent
///   at all already takes), matching a real OS re-parenting an orphan
///   rather than leaving it un-reapable.
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
        if let Slot::Occupied(process) = &mut sched.processes[index] {
            if process.parent == Some(exiting_pid) {
                process.parent = None;
            }
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
        sched.current[percpu::core_index()]
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
/// Nothing is its own parent, so `target` can never be `caller` itself.
/// On a single core that made this a pure "just finalize it" operation --
/// with more than one core, `target` might genuinely be `Running` on a
/// *different* core at this exact instant, so this first checks
/// `sched.current` for every core: if `target` isn't running anywhere,
/// this is still the same single-step finalize as before. If it is,
/// finalizing here immediately (and freeing its `AddressSpace`) while
/// that other core's CR3 still names it would be a genuine use-after-free
/// -- so instead this records an eviction request for that core, sends it
/// a targeted `RESCHEDULE_VECTOR` IPI, and bounded-spins on the
/// lock-free `PerCpuSlot.current` mirror (never contending `SCHEDULER`
/// while waiting) until that core's own IPI handler
/// ([`on_reschedule_ipi`]) confirms the eviction by clearing it.
///
/// That whole check-IPI-spin sequence runs in a loop, re-scanning from
/// scratch every time, rather than once: since every core now runs a
/// periodic preemption timer (see `arch::x86_64::lapic`'s
/// `arm_timer_this_core`), `target` can be preempted by its *own* core's
/// timer tick at any instant, pushed into the shared ready queue exactly
/// like any other preempted process, and picked up by a completely
/// different, third core before the IPI this function already sent even
/// arrives. When that happens, the spin-wait above still observes
/// `current` on the *original* core stop naming `target` (correctly --
/// it did leave that core) and returns, but `target` is now genuinely
/// running elsewhere, not evicted at all. A one-shot version of this
/// function would then finalize (and free the `AddressSpace` of) a
/// process another core's CR3 still actively names — a real,
/// reproduced bug (`xtask test-smp-kill-cross-core`, intermittently
/// after this milestone added per-core forced preemption, manifesting
/// as a page fault or an outright hang): the exact "stale CR3" hazard
/// this function's single-snapshot version already guarded against for
/// an *idle* core, just reachable now via a *live* one instead. Looping
/// closes it completely: this only ever proceeds to finalize once a scan
/// finds `target` not `current` on any core at all, however many times
/// it had to chase it across migrations to get there.
///
/// Crucially, that final "not running anywhere, safe to finalize" check
/// and the actual finalize ([`take_and_finalize_slot`]) happen under the
/// *same* `SCHEDULER` acquisition, not two separate ones -- an earlier
/// version of this loop scanned, released the lock, and only then called
/// [`terminate_slot`] (which re-locks separately), leaving a gap in
/// which a *different* core's own scheduling (another `on_timer_tick`,
/// or an idle core polling the ready queue) could pop `target` off the
/// ready queue and dispatch it as `current` before this function's own
/// second lock acquisition ran -- the exact same two-phase
/// check-then-act shape `wait_for_child`'s doc comment and
/// `docs/adr/0010`/`0011` already describe for other paths, just
/// reappearing here. Folding the check and the finalize into one locked
/// step closes it the same way those fixes do.
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

    loop {
        let mut pending_drops = Vec::new();
        let owning_core = {
            let mut sched = SCHEDULER.lock();
            match (0..MAX_CORES).find(|&core| sched.current[core] == Some(target)) {
                Some(core) => Some(core),
                None => {
                    // Not `current` anywhere at this exact locked instant
                    // -- finalize right here, before releasing the lock,
                    // so nothing can dispatch it onto a core in the gap a
                    // second, separate acquisition would otherwise leave
                    // open (see this function's doc comment).
                    take_and_finalize_slot(&mut sched, target, ExitStatus::Killed, &mut pending_drops);
                    None
                }
            }
        };
        // Outside the lock, exactly like `terminate_slot`: frees every
        // finalized `AddressSpace`'s physical frames, a variable-length
        // operation this codebase's convention keeps off the lock's
        // critical path.
        drop(pending_drops);

        let Some(core) = owning_core else {
            return Ok(());
        };

        percpu::slot(core)
            .evict_request
            .store(target.0, Ordering::Release);
        lapic::send_ipi(percpu::slot(core).lapic_id(), lapic::RESCHEDULE_VECTOR);

        // Mirrors `smp::bring_up_aps`'s own bounded-timeout wait pattern:
        // logged and finite, never a true infinite spin, even though in
        // practice the target core's IPI handler runs essentially
        // immediately. Deliberately much smaller than the AP bring-up
        // wait's own bound: with per-core forced preemption now in the
        // picture, `target` migrating away mid-wait is an expected,
        // ordinary occurrence (see this function's own doc comment), not
        // a rare failure -- this loop is meant to give up on a *stale*
        // wait quickly and let the outer loop re-scan and re-target,
        // not to burn a large fraction of a whole test scenario's wall
        //-clock budget spinning under slow TCG emulation on one attempt
        // that's already stale.
        const TIMEOUT_SPINS: u64 = 2_000_000;
        let mut spins = 0u64;
        let mut timed_out = false;
        while percpu::slot(core).current.load(Ordering::Acquire) == target.0 {
            core::hint::spin_loop();
            spins += 1;
            if spins > TIMEOUT_SPINS {
                timed_out = true;
                break;
            }
        }
        if timed_out {
            // Clear this core's own eviction request before moving on --
            // otherwise a *late* reschedule IPI for this exact request,
            // arriving after this function has already moved on to a
            // different core (or a different target migrated here since,
            // if this same core is later asked to evict someone else),
            // could be misread by `on_reschedule_ipi` as still relevant.
            // `evict_request` only ever names *one* outstanding request
            // per core, so clearing it here is always safe: either it
            // still names `target` (nothing consumed it -- the IPI truly
            // never arrived in time) or it was already zeroed by a
            // legitimate `on_reschedule_ipi` run that arrived just after
            // this loop gave up, in which case this store is a harmless
            // no-op racing a value that's already 0.
            percpu::slot(core).evict_request.store(0, Ordering::Release);
            crate::earlyprintln!(
                "[sched] core {core} did not evict pid {} in time -- re-scanning",
                target.index()
            );
        }
        // Loop back around and re-scan rather than assume a completed
        // wait means `target` is now safe to finalize -- see this
        // function's doc comment for why it might instead have simply
        // migrated.
    }
}

/// The `RESCHEDULE_VECTOR` IPI handler's scheduler-side half (see
/// `arch::x86_64::context_switch::ring3_reschedule`). Runs on whichever
/// core the IPI targeted, on that process's own kernel stack, with
/// `current_frame` pointing at its just-interrupted registers.
///
/// If this core has no pending eviction request, or the request no
/// longer names *this* core's current process (it already moved on by
/// the time the IPI landed -- e.g. it exited on its own first), this is a
/// no-op: return `current_frame` unchanged so the entry stub simply
/// resumes what was running. Otherwise, this process is being killed, not
/// preempted -- its frame isn't worth preserving -- so this clears
/// `current` (which is what unblocks `terminate_process`'s spin-wait
/// above) and falls into the same idle-or-pop-next-ready path every
/// reschedule IPI shares.
pub fn on_reschedule_ipi(current_frame: *mut TrapFrame) -> *mut TrapFrame {
    let core = percpu::core_index();
    let mut sched = SCHEDULER.lock();
    let evict = percpu::slot(core).evict_request.swap(0, Ordering::AcqRel);
    if evict == 0 || sched.current[core].map(|p| p.0) != Some(evict) {
        drop(sched);
        return current_frame;
    }
    set_current(&mut sched, core, None);
    switch_to_next_or_halt(sched, "[sched] evicted core found nothing else ready, halting.")
}

/// `SYS_WAIT`'s implementation. If `target` (a child of `caller`) has
/// already exited (`Slot::Zombie`), reaps it immediately and writes its
/// status into `current_frame`'s registers, returning it unchanged (the
/// non-blocking case). Otherwise, if `target` is still alive, records
/// `caller` as its `wait_waiter` -- so [`take_and_finalize_slot`] can
/// resolve it directly once `target` actually terminates, whatever the
/// cause -- and *immediately* blocks the caller itself
/// ([`block_current_process_locked`]), all under the one `SCHEDULER`
/// lock acquisition this function takes at the top.
///
/// That atomicity is load-bearing, not a style choice: an earlier version
/// of this function only recorded `wait_waiter` and returned a sentinel
/// telling `sys_wait` to separately call [`block_current_process`]
/// afterward -- two distinct lock acquisitions with a gap in between.
/// With more than one core, `target` terminating *in that exact gap* (on
/// a different core, entirely independently) would see `wait_waiter`
/// already set and immediately try to wake and resume `caller` -- mutating
/// its `trap_frame` and pushing it into the ready queue -- while `caller`
/// was still genuinely running right here, hadn't actually stopped, and
/// hadn't yet persisted *this* syscall's real trap frame anywhere. A real,
/// reproduced bug (`xtask test-smp-wait-cross-core`, the same one
/// `switch_to_next_or_halt`'s doc comment describes: a general-protection
/// fault at an unpredictable kernel address, or a flatly wrong exit status
/// delivered) that no amount of fixing what happens *after* blocking could
/// ever close, since the unsafe window was entirely *before* it. Since
/// only `target.parent` is ever permitted to become its `wait_waiter`, at
/// most one process can ever legitimately be waiting on a given target --
/// no wait *queue* is needed, just this one field. See
/// `docs/adr/0010-cross-core-scheduling.md`.
pub fn wait_for_child(current_frame: *mut TrapFrame, target: Pid, caller: Pid) -> *mut TrapFrame {
    let mut sched = SCHEDULER.lock();
    let index = target.index();
    if index >= MAX_PROCESSES || sched.generations[index] != target.generation() {
        drop(sched);
        unsafe { (*current_frame).rax = tarnos_abi::SyscallError::InvalidTarget.as_retval() as u64 };
        return current_frame;
    }

    enum Outcome {
        AlreadyDone(ExitStatus),
        MustBlock,
        Invalid,
    }
    let outcome = match &sched.processes[index] {
        Slot::Zombie { parent, status } if *parent == caller => Outcome::AlreadyDone(*status),
        Slot::Occupied(process) if process.parent == Some(caller) => Outcome::MustBlock,
        _ => Outcome::Invalid,
    };

    match outcome {
        Outcome::AlreadyDone(status) => {
            sched.processes[index] = Slot::Empty;
            drop(sched);
            let (kind, code) = status.to_regs();
            let regs = unsafe { &mut *current_frame };
            regs.rax = 0;
            regs.rdi = kind;
            regs.rsi = code;
            current_frame
        }
        Outcome::MustBlock => {
            if let Slot::Occupied(process) = &mut sched.processes[index] {
                process.wait_waiter = Some(caller);
            }
            block_current_process_locked(&mut sched, current_frame);
            switch_to_next_or_halt(sched, "[sched] every process blocked or exited, halting.")
        }
        Outcome::Invalid => {
            drop(sched);
            unsafe { (*current_frame).rax = tarnos_abi::SyscallError::InvalidTarget.as_retval() as u64 };
            current_frame
        }
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
        sched.current[percpu::core_index()]?
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

/// Writes a blocked process's saved registers for whatever just
/// completed its wait, and marks it `Ready`. Does not touch the ready
/// queue or idle cores — callers do that themselves once this returns
/// (both current callers need the process's own borrow to end first;
/// see NLL).
fn apply_wake_result(process: &mut Process, result: WakeResult) {
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
}

fn wake_blocked_process_locked(sched: &mut Inner, pid: Pid, result: WakeResult) {
    let Some(process) = occupied_mut(sched, pid) else {
        return;
    };
    // A `Waiter::Process(pid)` registered under `Endpoint::slot`'s lock
    // (or a `wait_waiter` about to call `wait_for_child`) is not
    // necessarily `Blocked` yet on more than one core: it may still be
    // between registering as a waiter and reaching its own
    // `block_current_process`/`wait_for_child` call, possibly on another
    // core entirely. A legitimate wake can only ever find this process
    // `Blocked` (the normal case) or still `Running` (this race window)
    // — never anything else, since only one rendezvous can be pending at
    // a time. Applying the wake now in the `Running` case would resume a
    // process that hasn't actually stopped executing; stash it instead,
    // for `block_current_process_locked` to apply the instant it
    // finishes the transition to `Blocked` — see `Process::pending_wake`.
    if process.state != ProcessState::Blocked {
        process.pending_wake = Some(result);
        return;
    }
    apply_wake_result(process, result);
    sched.ready.push(pid);
    // Safe to call while still holding `sched`'s lock: unlike
    // `SCHEDULER.lock()` itself, this only touches percpu atomics and
    // sends an IPI, never re-entering `SCHEDULER`.
    notify_idle_cores();
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

/// The mutation half of blocking the calling process, given `sched`'s
/// lock already held: persists `current_frame` into the process's own
/// `trap_frame` (so `wake_blocked_process` can resume it later, exactly
/// like a preempted process's frame is persisted in `on_timer_tick`),
/// marks it `Blocked`, and clears this core's `current` entry. Does
/// *not* push it back into the ready queue -- the one thing that
/// actually distinguishes this from an ordinary preemption -- so it can
/// never be scheduled again until something wakes it.
///
/// Factored out so [`wait_for_child`] can perform this as part of the
/// *same* lock acquisition that already found `target` still alive and
/// recorded `caller` as its `wait_waiter`, rather than that check and
/// this transition happening under two separate locks with a gap a
/// different core's wake could land in -- see that function's doc
/// comment.
///
/// Also resolves a [`Process::pending_wake`] the instant it appears:
/// if some other core already completed this process's rendezvous
/// before this point was reached (see that field's doc comment for the
/// exact race), the process is immediately handed back to `Ready`
/// instead of being left `Blocked` forever with nothing left to wake
/// it -- still entirely under this same lock acquisition, so there is
/// no window in which a fresh `wake_blocked_process` call could also
/// see it as `Blocked` and double-apply the wake.
fn block_current_process_locked(sched: &mut Inner, current_frame: *mut TrapFrame) {
    let core = percpu::core_index();
    if let Some(current_pid) = sched.current[core] {
        let mut woken = false;
        if let Slot::Occupied(process) = &mut sched.processes[current_pid.index()] {
            // SAFETY: `current_frame` is a valid, fully-initialized
            // TrapFrame -- it's the same frame the syscall entry
            // trampoline built for this process's own trap.
            process.trap_frame = unsafe { *current_frame };
            process.state = ProcessState::Blocked;
            if let Some(pending) = process.pending_wake.take() {
                apply_wake_result(process, pending);
                woken = true;
            }
        }
        if woken {
            sched.ready.push(current_pid);
            notify_idle_cores();
        }
        // Load-bearing, not cleanup: a blocked process is no longer
        // "current" on this core, and unlike the timer-tick path (which
        // either immediately overwrites `current` with whatever it
        // switches to next, or leaves it alone specifically *because*
        // the same process keeps running), `switch_to_next_or_halt`
        // below may find nothing ready and simply idle this core --
        // potentially for a long time. Leaving this core's `current`
        // stale-pointing at the now-blocked pid was a real bug: if this
        // process later gets woken and migrated to run on a *different*
        // core, this core's own `sched.current` entry stayed
        // `Some(pid)`. On the BSP that meant the very next PIT tick's
        // `on_timer_tick` (which trusts its own `sched.current[core]`
        // completely) would see this stale entry, overwrite the
        // process's real, *actively executing on another core* trapframe
        // with garbage from this core's own idle context, and push it
        // into the ready queue a second time -- corrupting its saved
        // state and setting up a second core to run the very same
        // process (and kernel stack) concurrently. Clearing it here
        // closes that window entirely.
        set_current(sched, core, None);
    }
}

/// Suspends the calling process inside a blocking `SYS_SEND`/`SYS_RECV`
/// that found no partner ready (see [`block_current_process_locked`] for
/// the actual transition), then switches to whatever's next ready --
/// exactly like `terminate_current_process`, including the same "nothing
/// left to run" halt if every other process is also blocked or gone.
///
/// `SYS_WAIT`'s equivalent is [`wait_for_child`], which performs the same
/// transition itself rather than calling this, so it can do so under the
/// *same* lock acquisition that decided blocking was necessary in the
/// first place -- see its doc comment for why that matters with more
/// than one core.
pub fn block_current_process(current_frame: *mut TrapFrame) -> *mut TrapFrame {
    let mut sched = SCHEDULER.lock();
    block_current_process_locked(&mut sched, current_frame);
    switch_to_next_or_halt(sched, "[sched] every process blocked or exited, halting.")
}

/// Starts running processes on the BSP: called exactly once, from boot
/// code, right after `main.rs` [`spawn`]s `init`.
///
/// This used to simply pop the ready queue and `expect` `init` to
/// actually be there — safe under single-core scheduling, where nothing
/// else could possibly touch the queue between `spawn` and this call.
/// With more than one core, that assumption no longer holds: every idle
/// AP is already busy-polling the very same shared queue (see
/// [`ap_enter_scheduler`]) the instant `main.rs`'s `spawn` call pushes
/// `init` into it, and can legitimately win the race and start running it
/// first — exactly the migration this milestone's shared-ready-queue
/// design accepts. So this is simply [`ap_enter_scheduler`] under a
/// boot-code-facing name: if `init` is still here, this core runs it; if
/// an AP already grabbed it, this core instead becomes the BSP's own
/// entry into the same busy-poll idle path every core shares, ready to
/// pick up whatever runs next. Never returns.
pub fn start() -> ! {
    ap_enter_scheduler()
}
