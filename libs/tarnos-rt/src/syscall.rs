//! Raw syscall wrappers. Register convention matches
//! `arch::x86_64::syscall` on the kernel side exactly: RAX=number, args
//! in RDI/RSI/RDX/R10/R8/R9, return in RAX (negative = error), RCX/R11
//! always marked clobbered since the `SYSCALL` instruction itself
//! overwrites them — no caller of these wrappers may assume otherwise.
use core::arch::asm;

use tarnos_abi::{CapIndex, Message, SyscallError, SYS_EXIT, SYS_RECV, SYS_SEND, SYS_YIELD};

pub fn sys_yield() {
    unsafe {
        asm!(
            "syscall",
            in("rax") SYS_YIELD,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
}

pub fn sys_send(cap: CapIndex, message: Message) -> Result<(), SyscallError> {
    let retval: i64;
    unsafe {
        asm!(
            "syscall",
            inout("rax") SYS_SEND => retval,
            in("rdi") cap.0 as u64,
            in("rsi") message.tag,
            in("rdx") message.words[0],
            in("r10") message.words[1],
            in("r8") message.words[2],
            in("r9") message.words[3],
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    if retval < 0 {
        Err(SyscallError::from_retval(retval))
    } else {
        Ok(())
    }
}

/// Receives a message on `cap`, blocking (at the kernel level — this
/// call simply doesn't return until a sender shows up) if none is
/// already waiting. `Err` only for a genuine failure: the capability
/// doesn't grant `RECV`, doesn't exist, or (see
/// `SyscallError::ResourceExhausted`) the endpoint's bounded wait queue
/// was already completely full.
pub fn sys_recv(cap: CapIndex) -> Result<Message, SyscallError> {
    let retval: i64;
    let tag: u64;
    let (w0, w1, w2): (u64, u64, u64);
    unsafe {
        asm!(
            "syscall",
            inout("rax") SYS_RECV => retval,
            inout("rdi") cap.0 as u64 => tag,
            out("rsi") w0,
            out("rdx") w1,
            out("r10") w2,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags)
        );
    }
    if retval < 0 {
        Err(SyscallError::from_retval(retval))
    } else {
        Ok(Message::new(tag, [w0, w1, w2, 0]))
    }
}

pub fn sys_exit(code: i32) -> ! {
    unsafe {
        asm!(
            "syscall",
            in("rax") SYS_EXIT,
            in("rdi") code as u64,
            options(noreturn)
        );
    }
}
