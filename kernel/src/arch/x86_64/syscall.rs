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
use crate::ipc::{KernelObjectRef, Rights};

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
        SYS_YIELD => crate::task::scheduler::on_syscall_yield(frame),
        SYS_SEND => {
            regs.rax = sys_send(regs) as u64;
            frame
        }
        SYS_RECV => {
            let (retval, tag, words) = sys_recv(regs);
            regs.rax = retval as u64;
            regs.rdi = tag;
            regs.rsi = words[0];
            regs.rdx = words[1];
            regs.r10 = words[2];
            frame
        }
        SYS_EXIT => crate::task::scheduler::on_syscall_exit(frame),
        _ => {
            regs.rax = SyscallError::NoSuchSyscall.as_retval() as u64;
            frame
        }
    }
}

fn sys_send(regs: &TrapFrame) -> i64 {
    let cap_index = CapIndex(regs.rdi as u32);
    let message = Message::new(regs.rsi, [regs.rdx, regs.r10, regs.r8, regs.r9]);

    let result = crate::task::scheduler::with_current_process(|process| {
        let slot = process.cap_table.lookup(cap_index, Rights::SEND)?;
        let KernelObjectRef::Endpoint(endpoint) = &slot.object;
        if endpoint.try_send(message, process.pid) {
            Ok(())
        } else {
            // Blocking send (no receiver waiting) is not implemented
            // this milestone — see ipc::endpoint's module docs. Every
            // path this milestone exercises guarantees a receiver is
            // already waiting, so this is not expected to trigger.
            Err(SyscallError::WouldBlock)
        }
    });

    match result {
        Some(Ok(())) => 0,
        Some(Err(e)) => e.as_retval(),
        None => SyscallError::BadCapability.as_retval(),
    }
}

/// Returns `(retval, tag, words[0..3])` — only 3 of the 4 message words
/// fit back in registers on return (RAX carries the status instead of a
/// 5th word), matching this milestone's demo, which only ever needs to
/// carry a short greeting string back out.
fn sys_recv(regs: &TrapFrame) -> (i64, u64, [u64; 3]) {
    let cap_index = CapIndex(regs.rdi as u32);

    let result = crate::task::scheduler::with_current_process(|process| {
        let slot = process.cap_table.lookup(cap_index, Rights::RECV)?;
        let KernelObjectRef::Endpoint(endpoint) = &slot.object;
        // Non-blocking only this milestone: a process blocking here
        // would need the scheduler to suspend it and later re-wake it
        // when a sender arrives, which nothing in this milestone's demo
        // exercises (see ipc::endpoint's module docs on `Waiter::Process`).
        endpoint.try_recv_nonblocking().ok_or(SyscallError::WouldBlock)
    });

    match result {
        Some(Ok(message)) => (
            0,
            message.tag,
            [message.words[0], message.words[1], message.words[2]],
        ),
        Some(Err(e)) => (e.as_retval(), 0, [0; 3]),
        None => (SyscallError::BadCapability.as_retval(), 0, [0; 3]),
    }
}
