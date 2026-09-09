//! SYSCALL/SYSRET entry point — the other half of the audited HAL
//! boundary alongside `context_switch`. Deliberately Linux-shaped at the
//! register/calling-convention level (RAX=number, args in
//! RDI/RSI/RDX/R10/R8/R9, negative RAX=`-errno`) even though the
//! semantics are entirely TarnOS-native this milestone: a future POSIX
//! shim can reuse this same trap trampoline and only swap the dispatch
//! table (see `docs/adr/0004-posix-abi-seam.md`).
//!
//! Unlike an interrupt, `SYSCALL` never switches stacks automatically —
//! there is no TSS mechanism for it — so the entry stub below does it by
//! hand, reading [`SYSCALL_KERNEL_RSP`] directly rather than through
//! per-CPU/GS-relative addressing: with a single core there is exactly
//! one "current kernel stack," so a plain global suffices.
use core::sync::atomic::{AtomicU64, Ordering};

use tarnos_abi::{CapIndex, Message, SyscallError, SYS_EXIT, SYS_RECV, SYS_SEND, SYS_YIELD};
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::VirtAddr;

use super::context_switch::TrapFrame;
use super::gdt;
use crate::ipc::endpoint::{RecvResult, SendResult};
use crate::ipc::{Endpoint, KernelObjectRef, Rights};
use crate::task::scheduler;
use crate::task::Pid;

/// The kernel stack top for whichever process is about to run — updated
/// by the scheduler on every switch, alongside `gdt::set_kernel_stack`.
pub static SYSCALL_KERNEL_RSP: AtomicU64 = AtomicU64::new(0);
/// Scratch cell for the user `RSP` at syscall entry, before this stub
/// switches onto the kernel stack — plain (not `SpinLock`) because
/// nothing else ever touches it: `SFMask` clears `IF` on entry, so this
/// single core cannot re-enter this trampoline before it finishes with
/// this value.
static SCRATCH_USER_RSP: AtomicU64 = AtomicU64::new(0);

static USER_CS_SELECTOR: AtomicU64 = AtomicU64::new(0);
static USER_SS_SELECTOR: AtomicU64 = AtomicU64::new(0);

pub fn set_syscall_kernel_stack(top: VirtAddr) {
    SYSCALL_KERNEL_RSP.store(top.as_u64(), Ordering::Relaxed);
}

/// Programs the MSRs `SYSCALL` needs: `STAR` (segment selectors, laid out
/// so this matches the GDT ordering fixed back when the GDT itself was
/// built — see `gdt`'s module doc comment), `LSTAR` (entry point), and
/// `SFMASK` (RFLAGS bits to clear on entry — just `IF`, so this stub runs
/// with interrupts off exactly like the timer entry stub does).
pub fn init() {
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
        LStar::write(syscall_entry_addr());
        SFMask::write(RFlags::INTERRUPT_FLAG);
    }
}

// Mirrors `context_switch`'s timer entry stub, with two differences
// forced by `SYSCALL` itself: there is no hardware-pushed frame to
// branch on (userspace is the only caller, always ring 3, so `cs`/`ss`
// are always the same known selectors) and no automatic stack switch (so
// this does it manually before touching anything else). Once on the
// kernel stack, it builds exactly the same `TrapFrame` shape the timer
// stub does, so `syscall_dispatch` and `resume` both work unmodified
// regardless of which trampoline produced the frame.
core::arch::global_asm!(
    ".global syscall_entry",
    "syscall_entry:",
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
    scratch_rsp = sym SCRATCH_USER_RSP,
    kernel_rsp = sym SYSCALL_KERNEL_RSP,
    user_cs = sym USER_CS_SELECTOR,
    user_ss = sym USER_SS_SELECTOR,
    dispatch = sym syscall_dispatch,
);

unsafe extern "C" {
    fn syscall_entry();
}

fn syscall_entry_addr() -> VirtAddr {
    VirtAddr::new(syscall_entry as *const () as u64)
}

#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(frame: *mut TrapFrame) -> *mut TrapFrame {
    let regs = unsafe { &mut *frame };
    match regs.rax {
        SYS_YIELD => scheduler::on_syscall_yield(frame),
        SYS_SEND => sys_send(frame),
        SYS_RECV => sys_recv(frame),
        SYS_EXIT => scheduler::on_syscall_exit(frame),
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
/// itself takes; doing both under one lock would be a single-core
/// self-deadlock the moment a wake-up needs that same lock.
fn resolve_endpoint(
    cap_index: CapIndex,
    required: Rights,
) -> Result<(alloc::sync::Arc<Endpoint>, Pid), SyscallError> {
    scheduler::with_current_process(|process| {
        let slot = process.cap_table.lookup(cap_index, required)?;
        let KernelObjectRef::Endpoint(endpoint) = &slot.object;
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
