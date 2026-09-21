//! SYSCALL/SYSRET entry point — the other half of the audited HAL
//! boundary alongside `context_switch`. Deliberately Linux-shaped at the
//! register/calling-convention level (RAX=number, args in
//! RDI/RSI/RDX/R10/R8/R9, negative RAX=`-errno`) even though the
//! semantics are entirely TarnOS-native this milestone: a future POSIX
//! shim can reuse this same trap trampoline and only swap the dispatch
//! table (see `docs/adr/0004-posix-abi-seam.md`).
//!
//! Unlike an interrupt, `SYSCALL` never switches stacks automatically —
//! there is no TSS mechanism for it — so the entry stub does it by hand,
//! reading a scratch cell directly rather than through the TSS. With more
//! than one core, "the current kernel stack" is genuinely per-core state,
//! and the entry stub's first two instructions touch `rsp` *before saving
//! a single register* — at that point `rax`/`rdx`/etc. still hold live
//! syscall arguments, so a `percpu::core_index()` call (which clobbers
//! exactly those registers via `CPUID`) cannot run there. Rather than
//! introduce a new per-core CPU register (`GS_BASE`/`swapgs`) just to
//! make that lookup safe at that one spot, this instead generates
//! [`percpu::MAX_CORES`] near-identical copies of the whole entry stub
//! (see [`syscall_entry_stub`]), each closed over its own dedicated pair
//! of scratch cells — safe because `LSTAR` (the MSR naming which stub a
//! given core's `SYSCALL` jumps to) is itself already a per-core MSR, the
//! same way each core already gets its own TSS (`gdt::TSS_TABLE`).
use core::sync::atomic::{AtomicU64, Ordering};

use tarnos_abi::{
    CapIndex, Message, SyscallError, PROGRAM_NAME_MAX, SYS_BLOCK_READ, SYS_EXIT, SYS_GRANT,
    SYS_KILL, SYS_PROCESS_START, SYS_RECV, SYS_SBRK, SYS_SEND, SYS_SPAWN, SYS_WAIT, SYS_YIELD,
};
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::structures::paging::{FrameAllocator, Page, PageTableFlags, Size4KiB};
use x86_64::VirtAddr;

use super::context_switch::TrapFrame;
use super::percpu;
use super::gdt;
use crate::driver::block::{BlockDevice, BlockError, SECTOR_SIZE};
use crate::driver::virtio_blk::{self, MAX_SECTORS_PER_REQUEST};
use crate::ipc::endpoint::{RecvResult, SendResult};
use crate::ipc::{CapabilitySlot, Endpoint, KernelObjectRef, Rights};
use crate::memory::phys::GlobalFrameAllocator;
use crate::memory::virt;
use crate::task::process::{ProcessState, USER_HEAP_START};
use crate::task::scheduler;
use crate::task::Pid;

/// Fixed per-process ceiling on total heap size, checked before any
/// frame is touched by [`sys_sbrk`]. Without this, a single process
/// requesting an astronomically large (but non-overflowing) increment
/// would have the kernel allocate physical frames until all of RAM is
/// committed to that one process's heap before finally failing on a
/// genuine OOM — a syscall-triggerable denial-of-service against every
/// other process. 64 MiB comfortably covers anything built so far.
const USER_HEAP_MAX_SIZE: u64 = 64 * 1024 * 1024;

fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

static USER_CS_SELECTOR: AtomicU64 = AtomicU64::new(0);
static USER_SS_SELECTOR: AtomicU64 = AtomicU64::new(0);

/// Generates one core's copy of the `SYSCALL` entry stub, plus its own
/// dedicated pair of scratch cells (the kernel-stack-top the scheduler
/// last set for this core, and a landing spot for the user `RSP` while
/// the stub isn't on any real stack yet). Mirrors `context_switch`'s
/// timer entry stub, with two differences forced by `SYSCALL` itself:
/// there is no hardware-pushed frame to branch on (userspace is the only
/// caller, always ring 3, so `cs`/`ss` are always the same known
/// selectors, shared across cores since the GDT's own selector numbers
/// are) and no automatic stack switch (so this does it manually before
/// touching anything else). Once on the kernel stack, it builds exactly
/// the same `TrapFrame` shape the timer stub does, so `syscall_dispatch`
/// and `resume` both work unmodified regardless of which trampoline
/// (which core) produced the frame.
///
/// Plain `AtomicU64`s, not `SpinLock`s: `SFMask` clears `IF` on entry, so
/// the *owning* core cannot re-enter its own copy of this trampoline
/// before it finishes with these values — and no other core ever touches
/// a copy that isn't its own.
macro_rules! syscall_entry_stub {
    ($entry_name:ident, $kernel_rsp:ident, $scratch_rsp:ident) => {
        pub static $kernel_rsp: AtomicU64 = AtomicU64::new(0);
        static $scratch_rsp: AtomicU64 = AtomicU64::new(0);

        core::arch::global_asm!(
            concat!(".global ", stringify!($entry_name)),
            concat!(stringify!($entry_name), ":"),
            "mov [rip + {scratch_rsp}], rsp",
            "mov rsp, [rip + {kernel_rsp}]",
            "push qword ptr [rip + {user_ss}]",
            "push qword ptr [rip + {scratch_rsp}]",
            "push r11", // rflags, saved here by SYSCALL
            "push qword ptr [rip + {user_cs}]",
            "push rcx", // rip, saved here by SYSCALL
            "push rax",
            "push rbx",
            "push rcx",
            "push rdx",
            "push rsi",
            "push rdi",
            "push rbp",
            "push r8",
            "push r9",
            "push r10",
            "push r11",
            "push r12",
            "push r13",
            "push r14",
            "push r15",
            "mov rdi, rsp",
            "call {dispatch}",
            "mov rsp, rax",
            "pop r15",
            "pop r14",
            "pop r13",
            "pop r12",
            "pop r11",
            "pop r10",
            "pop r9",
            "pop r8",
            "pop rbp",
            "pop rdi",
            "pop rsi",
            "pop rdx",
            "pop rcx",
            "pop rbx",
            "pop rax",
            "iretq",
            scratch_rsp = sym $scratch_rsp,
            kernel_rsp = sym $kernel_rsp,
            user_cs = sym USER_CS_SELECTOR,
            user_ss = sym USER_SS_SELECTOR,
            dispatch = sym syscall_dispatch,
        );

        unsafe extern "C" {
            fn $entry_name();
        }
    };
}

syscall_entry_stub!(syscall_entry_0, SYSCALL_KERNEL_RSP_0, SCRATCH_USER_RSP_0);
syscall_entry_stub!(syscall_entry_1, SYSCALL_KERNEL_RSP_1, SCRATCH_USER_RSP_1);
syscall_entry_stub!(syscall_entry_2, SYSCALL_KERNEL_RSP_2, SCRATCH_USER_RSP_2);
syscall_entry_stub!(syscall_entry_3, SYSCALL_KERNEL_RSP_3, SCRATCH_USER_RSP_3);
syscall_entry_stub!(syscall_entry_4, SYSCALL_KERNEL_RSP_4, SCRATCH_USER_RSP_4);
syscall_entry_stub!(syscall_entry_5, SYSCALL_KERNEL_RSP_5, SCRATCH_USER_RSP_5);
syscall_entry_stub!(syscall_entry_6, SYSCALL_KERNEL_RSP_6, SCRATCH_USER_RSP_6);
syscall_entry_stub!(syscall_entry_7, SYSCALL_KERNEL_RSP_7, SCRATCH_USER_RSP_7);

// One literal copy per possible core -- kept in sync with
// `percpu::MAX_CORES` by hand (`global_asm!` can't be generated in a
// loop), asserted at compile time just below rather than only at the
// array-length-mismatch error site.
const _: () = assert!(percpu::MAX_CORES == 8, "update syscall.rs's 8 entry-stub copies to match");

const ENTRY_ADDRS: [unsafe extern "C" fn(); percpu::MAX_CORES] = [
    syscall_entry_0,
    syscall_entry_1,
    syscall_entry_2,
    syscall_entry_3,
    syscall_entry_4,
    syscall_entry_5,
    syscall_entry_6,
    syscall_entry_7,
];

static KERNEL_RSP_SLOTS: [&AtomicU64; percpu::MAX_CORES] = [
    &SYSCALL_KERNEL_RSP_0,
    &SYSCALL_KERNEL_RSP_1,
    &SYSCALL_KERNEL_RSP_2,
    &SYSCALL_KERNEL_RSP_3,
    &SYSCALL_KERNEL_RSP_4,
    &SYSCALL_KERNEL_RSP_5,
    &SYSCALL_KERNEL_RSP_6,
    &SYSCALL_KERNEL_RSP_7,
];

fn syscall_entry_addr_for_core(core: usize) -> VirtAddr {
    VirtAddr::new(ENTRY_ADDRS[core] as usize as u64)
}

/// The kernel stack top for whichever process is about to run *on the
/// calling core* — updated by the scheduler on every switch, alongside
/// `gdt::set_kernel_stack`. Resolves its own core index internally
/// (mirroring `gdt::set_kernel_stack`'s own established pattern), so
/// every call site written back when there was only ever one core needed
/// no change at all.
pub fn set_syscall_kernel_stack(top: VirtAddr) {
    KERNEL_RSP_SLOTS[percpu::core_index()].store(top.as_u64(), Ordering::Relaxed);
}

/// Reads back this calling core's own `SYSCALL` kernel-stack scratch
/// cell — see [`set_syscall_kernel_stack`]'s own doc comment. Exists
/// purely for `task::scheduler::switch_to`'s own read-back assertion
/// (see `docs/adr/0023`), mirroring `gdt::kernel_stack`'s identical
/// purpose for `TSS.RSP0`.
pub fn syscall_kernel_stack() -> VirtAddr {
    VirtAddr::new(KERNEL_RSP_SLOTS[percpu::core_index()].load(Ordering::Relaxed))
}

/// Programs the MSRs `SYSCALL` needs: `STAR` (segment selectors, laid out
/// so this matches the GDT ordering fixed back when the GDT itself was
/// built — see `gdt`'s module doc comment), `LSTAR` (this core's own
/// entry-stub copy — see [`syscall_entry_stub`]), and `SFMASK` (RFLAGS
/// bits to clear on entry — just `IF`, so this stub runs with interrupts
/// off exactly like the timer entry stub does).
///
/// Must be called once by every core, BSP and every AP alike (`LSTAR` is
/// genuinely per-core hardware state) — `STAR`/`EFER`/`SFMASK` end up
/// holding numerically identical values on every core (the GDT selector
/// layout is shared), but they're still per-core MSRs with no
/// "set once for every core" mechanism, so this just reruns the whole
/// idempotent sequence on each caller rather than trying to special-case
/// which parts only need doing once.
pub fn init() {
    let core = percpu::core_index();
    let selectors = gdt::selectors();
    USER_CS_SELECTOR.store(selectors.user_code.0 as u64, Ordering::Relaxed);
    USER_SS_SELECTOR.store(selectors.user_data.0 as u64, Ordering::Relaxed);

    unsafe {
        Efer::update(|flags| *flags |= EferFlags::SYSTEM_CALL_EXTENSIONS);
        Star::write(
            selectors.user_code,
            selectors.user_data,
            selectors.kernel_code,
            selectors.kernel_data,
        )
        .expect("GDT selector layout does not satisfy SYSCALL/SYSRET's fixed-offset requirement");
        LStar::write(syscall_entry_addr_for_core(core));
        SFMask::write(RFlags::INTERRUPT_FLAG);
    }
}

#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(frame: *mut TrapFrame) -> *mut TrapFrame {
    let regs = unsafe { &mut *frame };
    match regs.rax {
        SYS_YIELD => scheduler::on_syscall_yield(frame),
        SYS_SEND => sys_send(frame),
        SYS_RECV => sys_recv(frame),
        SYS_EXIT => scheduler::on_syscall_exit(frame),
        SYS_SPAWN => sys_spawn(frame),
        SYS_GRANT => sys_grant(frame),
        SYS_PROCESS_START => sys_process_start(frame),
        SYS_WAIT => sys_wait(frame),
        SYS_KILL => sys_kill(frame),
        SYS_SBRK => sys_sbrk(frame),
        SYS_BLOCK_READ => sys_block_read(frame),
        _ => {
            regs.rax = SyscallError::NoSuchSyscall.as_retval() as u64;
            frame
        }
    }
}

/// Resolves `cap_index` against the calling process's own table with
/// `required` rights, returning a cloned handle to the endpoint plus the
/// calling `Pid`. Kept to a short, self-contained critical section
/// (borrowing the process only long enough to clone an `Arc` out of it)
/// specifically so the actual send/recv below — which may call back into
/// `task::scheduler` to wake a *different* blocked process — never runs
/// while still holding the scheduler's lock `with_current_process`
/// itself takes; doing both under one lock would be a same-core
/// self-deadlock (this core re-entering a lock it already holds) the
/// moment a wake-up needs that same lock — a hazard tied to holding the
/// lock across a reentrant call, not to how many cores exist; a second
/// core spinning on the same lock would only make it worse (real
/// contention on top of the same-core reentrancy), never better.
fn resolve_endpoint(
    cap_index: CapIndex,
    required: Rights,
) -> Result<(alloc::sync::Arc<Endpoint>, Pid), SyscallError> {
    scheduler::with_current_process(|process| {
        let slot = process.cap_table.lookup(cap_index, required)?;
        let KernelObjectRef::Endpoint(endpoint) = &slot.object else {
            return Err(SyscallError::BadCapability);
        };
        Ok((endpoint.clone(), process.pid))
    })
    .unwrap_or(Err(SyscallError::BadCapability))
}

fn sys_send(frame: *mut TrapFrame) -> *mut TrapFrame {
    let regs = unsafe { &mut *frame };
    let cap_index = CapIndex(regs.rdi as u32);
    let message = Message::new(regs.rsi, [regs.rdx, regs.r10, regs.r8, regs.r9]);

    let (endpoint, pid) = match resolve_endpoint(cap_index, Rights::SEND) {
        Ok(pair) => pair,
        Err(e) => {
            regs.rax = e.as_retval() as u64;
            return frame;
        }
    };

    match endpoint.try_send(message, pid) {
        SendResult::Delivered => {
            regs.rax = 0;
            frame
        }
        // No receiver was ready: this process is now queued as a
        // waiting sender and must actually suspend until one arrives.
        SendResult::Blocked => scheduler::block_current_process(frame),
        SendResult::QueueFull => {
            regs.rax = SyscallError::ResourceExhausted.as_retval() as u64;
            frame
        }
    }
}

fn sys_recv(frame: *mut TrapFrame) -> *mut TrapFrame {
    let regs = unsafe { &mut *frame };
    let cap_index = CapIndex(regs.rdi as u32);

    let (endpoint, pid) = match resolve_endpoint(cap_index, Rights::RECV) {
        Ok(pair) => pair,
        Err(e) => {
            regs.rax = e.as_retval() as u64;
            return frame;
        }
    };

    match endpoint.try_recv(pid) {
        RecvResult::Delivered(message) => {
            regs.rax = 0;
            regs.rdi = message.tag;
            regs.rsi = message.words[0];
            regs.rdx = message.words[1];
            regs.r10 = message.words[2];
            frame
        }
        // No sender was ready: this process is now queued as a waiting
        // receiver and must actually suspend until one arrives.
        RecvResult::Blocked => scheduler::block_current_process(frame),
        RecvResult::QueueFull => {
            regs.rax = SyscallError::ResourceExhausted.as_retval() as u64;
            frame
        }
    }
}

/// `SYS_SPAWN`: creates a new, `Suspended` process from a boot-shipped
/// program named by `rdi`/`rsi`/`rdx` (packed via
/// `tarnos_abi::pack_program_name`), recording the caller as its parent.
/// Never blocks — always returns `frame` directly. On success, `rax`
/// holds the new process's raw `Pid`; there is no capability wrapping it
/// (a `Pid` alone confers no authority — only `SYS_GRANT`/
/// `SYS_PROCESS_START`'s parent-of-a-Suspended-child check does), so
/// there is nothing to guard against a forged value here beyond what
/// those two syscalls already check.
fn sys_spawn(frame: *mut TrapFrame) -> *mut TrapFrame {
    let regs = unsafe { &mut *frame };
    let mut name_buf = [0u8; PROGRAM_NAME_MAX];
    let name = tarnos_abi::unpack_program_name(regs.rdi, regs.rsi, regs.rdx, &mut name_buf);

    let result: Result<u64, SyscallError> = (|| {
        let caller_pid = scheduler::with_current_process(|p| p.pid)
            .ok_or(SyscallError::InvalidTarget)?;
        let elf_bytes =
            crate::task::process::lookup_spawnable_module(name).ok_or(SyscallError::NoSuchProgram)?;
        let child_pid = scheduler::allocate_pid();
        let mut child =
            match crate::task::process::Process::from_elf(child_pid, elf_bytes, Some(caller_pid)) {
                Ok(child) => child,
                Err(_) => {
                    // `allocate_pid` already reserved `child_pid`'s slot;
                    // since nothing will ever call `spawn_suspended` for
                    // it now, give it back rather than leaking it as a
                    // permanently `Reserved` slot -- see
                    // `scheduler::release_reservation`'s doc comment.
                    scheduler::release_reservation(child_pid);
                    return Err(SyscallError::SpawnFailed);
                }
            };
        child.state = ProcessState::Suspended;
        scheduler::spawn_suspended(child)?;
        Ok(child_pid.0)
    })();

    regs.rax = match result {
        Ok(pid) => pid,
        Err(e) => e.as_retval() as u64,
    };
    frame
}

/// `SYS_GRANT`: clones a capability from the caller's own table into
/// `target_pid`'s table, narrowed to `requested` rights. Only permitted
/// while `target_pid` is a `Suspended` child of the caller. Resolves the
/// caller's own capability first, in its own short critical section
/// (mirroring [`resolve_endpoint`]'s reasoning), fully releasing that
/// lock before a second, separate `with_process` call touches the
/// target — the two scheduler-lock acquisitions never nest.
fn sys_grant(frame: *mut TrapFrame) -> *mut TrapFrame {
    let regs = unsafe { &mut *frame };
    let target_pid = Pid(regs.rdi);
    let src_cap = CapIndex(regs.rsi as u32);
    let dest_cap = CapIndex(regs.rdx as u32);
    let requested = Rights::from_bits_truncate(regs.r10 as u8);

    let resolved: Result<(KernelObjectRef, Pid), SyscallError> = scheduler::with_current_process(
        |process| {
            let slot = process.cap_table.get(src_cap)?;
            if !slot.rights.contains(requested) {
                return Err(SyscallError::PermissionDenied);
            }
            Ok((slot.object.clone(), process.pid))
        },
    )
    .unwrap_or(Err(SyscallError::BadCapability));

    let outcome = resolved.and_then(|(object, caller_pid)| {
        scheduler::with_process(target_pid, |child| {
            if child.parent != Some(caller_pid) || child.state != ProcessState::Suspended {
                return Err(SyscallError::InvalidTarget);
            }
            child.cap_table.insert(
                dest_cap,
                CapabilitySlot {
                    object,
                    rights: requested,
                },
            );
            Ok(())
        })
        .unwrap_or(Err(SyscallError::InvalidTarget))
    });

    regs.rax = match outcome {
        Ok(()) => 0,
        Err(e) => e.as_retval() as u64,
    };
    frame
}

/// `SYS_PROCESS_START`: releases a `Suspended` child of the caller into
/// the scheduler's ready queue. See [`scheduler::start_child`] for the
/// ownership check.
fn sys_process_start(frame: *mut TrapFrame) -> *mut TrapFrame {
    let regs = unsafe { &mut *frame };
    let target_pid = Pid(regs.rdi);

    let outcome = scheduler::with_current_process(|p| p.pid)
        .ok_or(SyscallError::InvalidTarget)
        .and_then(|caller_pid| scheduler::start_child(target_pid, caller_pid));

    regs.rax = match outcome {
        Ok(()) => 0,
        Err(e) => e.as_retval() as u64,
    };
    frame
}

/// `SYS_WAIT`: blocks until `target_pid` (a child of the caller, in any
/// state) terminates, then returns its exit status —
/// `rdi`=kind (`0`=`Exited`, `1`=`Faulted`, `2`=`Killed`), `rsi`=code
/// (`Exited` only). Non-blocking if `target_pid` already terminated
/// before this call. See [`scheduler::wait_for_child`], which performs
/// the "already done, or must block" decision and the blocking
/// transition itself as a single atomic step -- unlike every other
/// blocking syscall here, this can't be split into a separate check and
/// a separate `scheduler::block_current_process` call (see that
/// function's doc comment for why).
fn sys_wait(frame: *mut TrapFrame) -> *mut TrapFrame {
    let regs = unsafe { &mut *frame };
    let target_pid = Pid(regs.rdi);

    match scheduler::with_current_process(|p| p.pid) {
        Some(caller_pid) => scheduler::wait_for_child(frame, target_pid, caller_pid),
        None => {
            regs.rax = SyscallError::InvalidTarget.as_retval() as u64;
            frame
        }
    }
}

/// `SYS_KILL`: immediately terminates `target_pid`, a child of the
/// caller, regardless of its current state. See
/// [`scheduler::terminate_process`] for the ownership check. Never
/// blocks or switches — a caller can never target itself (see that
/// function's doc comment) — so this always returns `frame` directly.
fn sys_kill(frame: *mut TrapFrame) -> *mut TrapFrame {
    let regs = unsafe { &mut *frame };
    let target_pid = Pid(regs.rdi);

    let outcome = scheduler::with_current_process(|p| p.pid)
        .ok_or(SyscallError::InvalidTarget)
        .and_then(|caller_pid| scheduler::terminate_process(target_pid, caller_pid));

    regs.rax = match outcome {
        Ok(()) => 0,
        Err(e) => e.as_retval() as u64,
    };
    frame
}

/// `SYS_SBRK`: grows the caller's heap by `increment` (`rdi`, as `i64`)
/// bytes and returns the previous break, or a negative `SyscallError` —
/// `increment == 0` is a side-effect-free query.
///
/// Grow-only this milestone: a negative `increment` is rejected rather
/// than actually shrinking (see `docs/adr/0008`). `increment` is also
/// rejected if it would overflow the break address or grow the heap
/// past [`USER_HEAP_MAX_SIZE`] — checked *before* touching the frame
/// allocator, so an adversarial request never allocates anything before
/// being rejected.
///
/// Deliberately does the frame-allocation-and-mapping loop *outside*
/// `with_current_process`/`SCHEDULER` — two short, separate critical
/// sections (validate-and-reserve, then commit) around it instead of
/// one that spans the whole loop. An earlier version held `SCHEDULER`
/// across the entire loop, on the reasoning that the lock order
/// `SCHEDULER` -> phys-allocator has no cycle so nesting is safe; that
/// reasoning was correct about *safety* but missed *liveness*: a
/// multi-page grow (each iteration allocating a frame and walking page
/// tables) held `SCHEDULER` for however long that took, and every other
/// core's `on_timer_tick`/`on_syscall_yield`/`wake_blocked_process` — all
/// of which need the same lock — stalled behind it the whole time. Never
/// visibly wrong on any earlier single-workload heap-growth test, but a
/// real, reproduced bug once something ran heap growth *concurrently*
/// with other cores under constant forced preemption (found while
/// prototyping a combined multi-workload stress scenario, deferred to a
/// later milestone — see `docs/adr/0011`): those cores' scheduling could
/// stall for the entire grow, and (with more than one such lock-holding
/// stretch overlapping across cores) the resulting head-of-line
/// blocking was severe enough to look like a hang.
///
/// Splitting the critical section is sound because only the calling
/// process's own single execution thread ever touches its own
/// `heap_end` or extends its own `AddressSpace` — nothing else can
/// observe or race the gap between the two acquisitions. A concurrent
/// `SYS_KILL` targeting this same process is still safe: it finds this
/// process still `current` on this core (unchanged by releasing
/// `SCHEDULER` here) and correctly waits for this syscall to finish
/// before tearing anything down, exactly like it would for any other
/// in-progress syscall.
fn sys_sbrk(frame: *mut TrapFrame) -> *mut TrapFrame {
    let regs = unsafe { &mut *frame };
    let increment = regs.rdi as i64;

    let outcome: Result<u64, SyscallError> = (|| {
        let (old_end, new_end, old_top, new_top, pml4_frame) = scheduler::with_current_process(
            |p| {
                if increment < 0 {
                    return Err(SyscallError::InvalidArgument);
                }
                let old_end = p.heap_end;
                let new_end = old_end
                    .checked_add(increment as u64)
                    .ok_or(SyscallError::InvalidArgument)?;
                if new_end - USER_HEAP_START > USER_HEAP_MAX_SIZE {
                    return Err(SyscallError::InvalidArgument);
                }
                let old_top = align_up(old_end, 4096);
                let new_top = align_up(new_end, 4096);
                Ok((old_end, new_end, old_top, new_top, p.address_space.pml4_frame()))
            },
        )
        .unwrap_or(Err(SyscallError::InvalidTarget))?;

        let flags = PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::NO_EXECUTE;
        let mut addr = old_top;
        while addr < new_top {
            let mut allocator = GlobalFrameAllocator;
            let new_frame = allocator
                .allocate_frame()
                .ok_or(SyscallError::ResourceExhausted)?;
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));
            // SAFETY: `pml4_frame` is this same, still-live process's own
            // PML4 (nothing else can drop it -- see this function's doc
            // comment on why the gap since the first `with_current_process`
            // call is safe).
            unsafe { crate::memory::virt::map_in(pml4_frame, page, new_frame, flags) }
                .map_err(|_| SyscallError::ResourceExhausted)?;
            addr += 4096;
        }

        scheduler::with_current_process(|p| p.heap_end = new_end)
            .ok_or(SyscallError::InvalidTarget)?;
        Ok(old_end)
    })();

    regs.rax = match outcome {
        Ok(old_end) => old_end,
        Err(e) => e.as_retval() as u64,
    };
    frame
}

/// `SYS_BLOCK_READ`: reads `sector_count` (`r10`) whole sectors starting
/// at `lba` (`rsi`) from the block device named by `cap_index` (`rdi`)
/// into the caller's own buffer at `buf_ptr` (`rdx`). Never blocks (the
/// driver polls synchronously to completion internally) — always
/// returns `frame` directly.
///
/// This kernel's first syscall that writes through a caller-supplied
/// pointer — see `memory::virt::translate_in`'s own doc comment for why
/// that needs a dedicated page-table walk in the *caller's* own address
/// space, not the global kernel-only mapper every other memory helper
/// here uses. Every page the destination range touches is validated
/// (present, writable, user-accessible) *before* the device is ever
/// touched — an invalid range costs nothing but the walk itself.
///
/// Resolves the capability and validates the buffer in one short
/// critical section (mirroring `resolve_endpoint`/`sys_sbrk`'s own
/// reasoning: never hold `SCHEDULER` across the driver's own polling
/// loop, however brief in practice), then performs the actual device
/// I/O and the copy into the caller's buffer entirely outside that lock.
fn sys_block_read(frame: *mut TrapFrame) -> *mut TrapFrame {
    let regs = unsafe { &mut *frame };
    let cap_index = CapIndex(regs.rdi as u32);
    let lba = regs.rsi;
    let buf_ptr = regs.rdx;
    let sector_count = regs.r10;

    let outcome: Result<(), SyscallError> = (|| {
        if sector_count == 0 || sector_count as usize > MAX_SECTORS_PER_REQUEST {
            return Err(SyscallError::InvalidArgument);
        }
        let len = sector_count * SECTOR_SIZE as u64;
        let end = buf_ptr.checked_add(len).ok_or(SyscallError::InvalidArgument)?;

        let pml4_frame = scheduler::with_current_process(|process| {
            let slot = process.cap_table.lookup(cap_index, Rights::READ)?;
            match &slot.object {
                KernelObjectRef::BlockDevice => Ok(process.address_space.pml4_frame()),
                KernelObjectRef::Endpoint(_) => Err(SyscallError::BadCapability),
            }
        })
        .unwrap_or(Err(SyscallError::BadCapability))?;

        let required =
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        let mut page_addr = buf_ptr & !0xFFF;
        while page_addr < end {
            // SAFETY: `pml4_frame` names this same, still-live calling
            // process's own PML4 -- this syscall handler is the only
            // thing acting on it meanwhile (same reasoning `sys_sbrk`'s
            // own doc comment gives for its own gap between
            // `with_current_process` calls).
            let (_, flags) = unsafe { virt::translate_in(pml4_frame, VirtAddr::new(page_addr)) }
                .ok_or(SyscallError::InvalidArgument)?;
            if !flags.contains(required) {
                return Err(SyscallError::InvalidArgument);
            }
            page_addr = page_addr.saturating_add(4096);
        }

        let mut kernel_buf = [0u8; MAX_SECTORS_PER_REQUEST * SECTOR_SIZE];
        match virtio_blk::with_device(|device| device.read_sectors(lba, &mut kernel_buf[..len as usize]))
        {
            Some(Ok(())) => {}
            Some(Err(BlockError::OutOfRange)) => return Err(SyscallError::IoOutOfRange),
            Some(Err(_)) => return Err(SyscallError::IoError),
            None => return Err(SyscallError::IoError),
        }

        // Copy into the caller's buffer one physical page at a time,
        // through each page's own HHDM alias -- never a raw write
        // through `buf_ptr` itself. That virtual address is only
        // meaningful under the *caller's* own page tables; going
        // through the physical alias instead means this doesn't
        // silently depend on "SYSCALL never switches CR3" (true on this
        // kernel today, but not a fact this function needs to lean on).
        let mut page_addr = buf_ptr & !0xFFF;
        while page_addr < end {
            // SAFETY: re-translated, not reused from the validation pass
            // above -- cheap (a page-table walk, not I/O), and nothing
            // in between could have changed this process's own mappings
            // anyway. Expect, not `?`: this exact address was already
            // confirmed mapped above; a failure here would mean this
            // process's own page tables changed underneath this single
            // syscall, which nothing does.
            let (phys_page_base, _) =
                unsafe { virt::translate_in(pml4_frame, VirtAddr::new(page_addr)) }
                    .expect("page was already validated as mapped above");
            let copy_start = buf_ptr.max(page_addr);
            let copy_end = end.min(page_addr.saturating_add(4096));
            let offset_in_page = copy_start - page_addr;
            let offset_in_kernel_buf = copy_start - buf_ptr;
            let copy_len = (copy_end - copy_start) as usize;
            let dest = virt::phys_to_virt(phys_page_base + offset_in_page);
            // SAFETY: `dest` is this exact page's own HHDM alias
            // (ordinary RAM, not device MMIO), `copy_len` bytes of which
            // were just confirmed present/writable/user-accessible
            // above; `kernel_buf` is this function's own local array,
            // read-only from this point on.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    kernel_buf.as_ptr().add(offset_in_kernel_buf as usize),
                    dest.as_mut_ptr::<u8>(),
                    copy_len,
                );
            }
            page_addr = page_addr.saturating_add(4096);
        }

        Ok(())
    })();

    regs.rax = match outcome {
        Ok(()) => 0,
        Err(e) => e.as_retval() as u64,
    };
    frame
}
