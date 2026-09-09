//! The audited HAL boundary for entering/leaving a process: a hand-written
//! interrupt entry stub (not the `extern "x86-interrupt"` ABI used
//! elsewhere, which hides general-purpose registers from us — a
//! scheduler that might resume a *different* process than the one just
//! interrupted needs full control over exactly what gets saved and
//! restored) plus [`resume`], the single `iretq`-from-[`TrapFrame`]
//! routine used both for a process's very first entry and for every
//! later preemption-resume. There is no special-cased "first entry" path.
use x86_64::VirtAddr;

use super::gdt;

/// Saved CPU state for one process, captured by the timer entry stub and
/// consumed by [`resume`]. Field order is load-bearing: it must match
/// exactly the push order in `timer_interrupt_entry` (below) and the pop
/// order in [`resume`] — `r15` first (lowest address, where `rsp` ends up
/// pointing after the entry stub's 15 pushes) through `rax`, followed by
/// the hardware-defined trap frame `rip`/`cs`/`rflags`/`rsp`/`ss`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// x86 RFLAGS: reserved bit 1 (always set) | IF (interrupt flag).
const RFLAGS_IF_AND_RESERVED: u64 = 0x202;

impl TrapFrame {
    /// Builds the frame for a process's very first entry into ring 3, as
    /// if it had already trapped in once. Deliberately produces exactly
    /// the same shape [`resume`] expects for any other preemption-resume
    /// — first entry is not a separate code path, just a frame
    /// constructed instead of captured.
    pub fn initial_user_frame(entry: VirtAddr, user_stack_top: VirtAddr) -> Self {
        let selectors = gdt::selectors();
        Self {
            rip: entry.as_u64(),
            cs: selectors.user_code.0 as u64,
            rflags: RFLAGS_IF_AND_RESERVED,
            rsp: user_stack_top.as_u64(),
            ss: selectors.user_data.0 as u64,
            ..Default::default()
        }
    }
}

/// Saved CPU state captured by one of the process-facing exception entry
/// stubs (below) for a vector that carries a hardware error code — page
/// fault and general-protection fault. Identical to [`TrapFrame`] except
/// for the extra `error_code` word hardware pushes directly below `rip`
/// for these vectors; kept as a separate type (rather than an `Option`
/// field on `TrapFrame`) so `TrapFrame`'s own layout — load-bearing for
/// every non-faulting resume path — never has to account for a field
/// that's meaningless outside a fault.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FaultFrameWithCode {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

// The timer-interrupt entry point. Two paths, chosen by checking the RPL
// of the hardware-saved CS (always at a fixed offset regardless of which
// path was taken, since it's the second-to-last thing pushed by hardware
// either way):
//
// - Interrupted from ring 3 (a process was running): capture the full
//   register state into a `TrapFrame` on the current (per-process)
//   kernel stack, hand it to `ring3_timer_tick`, and resume whatever
//   `TrapFrame` it returns — the same process, or a different one the
//   scheduler picked instead. Either way exits through the same
//   pop-everything-then-`iretq` tail.
// - Interrupted from ring 0 (the kernel itself: the idle loop, a kernel
//   task, boot code): there is no process to preempt *away from* — the
//   kernel is not a schedulable entity — so this path only does tick
//   bookkeeping and always resumes exactly what was interrupted. Using
//   the scheduler path here would be wrong, not just unnecessary: it
//   would require synthesizing an `SS`/`RSP` hardware frame that a
//   same-privilege interrupt never pushed in the first place.
//
// `iretq` itself needs no branching to handle both cases: it always pops
// RIP/CS/RFLAGS, and pops RSP/SS *in addition* only if the popped CS's
// CPL differs from the current one — exactly matching whichever case
// hardware already encoded by how many words it pushed on entry.
core::arch::global_asm!(
    ".global timer_interrupt_entry",
    "timer_interrupt_entry:",
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
    // 15 pushed qwords (120 bytes) + RIP (8 bytes) = CS at offset 128.
    "mov rax, [rsp + 128]",
    "test al, 3",
    "jz 2f",
    "1:", // interrupted from ring 3
    "mov rdi, rsp",
    "call {ring3_tick}",
    "mov rsp, rax",
    "jmp 3f",
    "2:", // interrupted from ring 0
    "call {ring0_tick}",
    "3:",
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
    ring3_tick = sym ring3_timer_tick,
    ring0_tick = sym ring0_timer_tick,
);

unsafe extern "C" {
    fn timer_interrupt_entry();
}

/// The address to install in the IDT for the timer vector.
pub fn timer_interrupt_entry_addr() -> VirtAddr {
    VirtAddr::new(timer_interrupt_entry as *const () as u64)
}

#[unsafe(no_mangle)]
extern "C" fn ring3_timer_tick(frame: *mut TrapFrame) -> *mut TrapFrame {
    super::interrupts::on_timer_tick_bookkeeping();
    let next = crate::task::scheduler::on_timer_tick(frame);
    super::interrupts::send_timer_eoi();
    next
}

#[unsafe(no_mangle)]
extern "C" fn ring0_timer_tick() {
    super::interrupts::on_timer_tick_bookkeeping();
    super::interrupts::send_timer_eoi();
}

// Process-facing exception entry stubs: page fault, general-protection
// fault, invalid opcode, divide error. Each needs the same hand-rolled
// register-capture treatment as the timer above and for the same
// reason — killing the faulting process and switching to whatever the
// scheduler picks next needs full control over exactly which registers
// get restored on the way out, which the compiler-generated
// `extern "x86-interrupt"` ABI (used for every *other* exception here,
// none of which need to redirect control anywhere but back to where
// they fired) does not give us.
//
// Two shapes, not four, since only the presence of a hardware error
// code affects the stack layout: `exception_entry_no_code` calls its
// two Rust functions with a `*mut TrapFrame` (CS lands at the same
// offset as the timer stub's, since neither pushes an error code);
// `exception_entry_with_code` calls them with a `*mut FaultFrameWithCode`
// instead (CS is 8 bytes further out, past the error code). Both branch
// on the saved CS's CPL exactly like the timer stub: CPL 0 means the
// *kernel's own* code faulted, which is still simply fatal (the ring0
// function never returns), while CPL 3 means a process faulted, which
// the ring3 function turns into "kill that process, resume whatever the
// scheduler picks next" — reusing the same
// `task::scheduler::terminate_current_process` used for an ordinary
// `SYS_EXIT`.
macro_rules! exception_entry_no_code {
    ($entry:ident, $ring0:path, $ring3:path) => {
        core::arch::global_asm!(
            concat!(".global ", stringify!($entry)),
            concat!(stringify!($entry), ":"),
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
            // Same offset as the timer stub: 15 pushed qwords (120
            // bytes) + RIP (8 bytes) = CS at offset 128.
            "mov rax, [rsp + 128]",
            "test al, 3",
            "jz 2f",
            "1:", // ring3: a process faulted
            "mov rdi, rsp",
            "call {ring3}",
            "mov rsp, rax",
            "jmp 3f",
            "2:", // ring0: the kernel itself faulted -- never returns
            "mov rdi, rsp",
            "call {ring0}",
            "3:",
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
            ring3 = sym $ring3,
            ring0 = sym $ring0,
        );
    };
}

macro_rules! exception_entry_with_code {
    ($entry:ident, $ring0:path, $ring3:path) => {
        core::arch::global_asm!(
            concat!(".global ", stringify!($entry)),
            concat!(stringify!($entry), ":"),
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
            // 120 bytes of pushes + error_code (8) + RIP (8) = CS at
            // offset 136 -- 8 further out than the no-error-code stub,
            // to account for the hardware-pushed error code.
            "mov rax, [rsp + 136]",
            "test al, 3",
            "jz 2f",
            "1:", // ring3: a process faulted
            "mov rdi, rsp",
            "call {ring3}",
            "mov rsp, rax",
            "jmp 3f",
            "2:", // ring0: the kernel itself faulted -- never returns
            "mov rdi, rsp",
            "call {ring0}",
            "3:",
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
            ring3 = sym $ring3,
            ring0 = sym $ring0,
        );
    };
}

exception_entry_no_code!(
    divide_error_entry,
    super::idt::divide_error_ring0,
    super::idt::divide_error_ring3
);
exception_entry_no_code!(
    invalid_opcode_entry,
    super::idt::invalid_opcode_ring0,
    super::idt::invalid_opcode_ring3
);
exception_entry_with_code!(
    page_fault_entry,
    super::idt::page_fault_ring0,
    super::idt::page_fault_ring3
);
exception_entry_with_code!(
    general_protection_fault_entry,
    super::idt::general_protection_fault_ring0,
    super::idt::general_protection_fault_ring3
);

unsafe extern "C" {
    fn divide_error_entry();
    fn invalid_opcode_entry();
    fn page_fault_entry();
    fn general_protection_fault_entry();
}

/// The addresses to install in the IDT for the four process-facing
/// exception vectors handled by the hand-rolled stubs above.
pub fn divide_error_entry_addr() -> VirtAddr {
    VirtAddr::new(divide_error_entry as *const () as u64)
}
pub fn invalid_opcode_entry_addr() -> VirtAddr {
    VirtAddr::new(invalid_opcode_entry as *const () as u64)
}
pub fn page_fault_entry_addr() -> VirtAddr {
    VirtAddr::new(page_fault_entry as *const () as u64)
}
pub fn general_protection_fault_entry_addr() -> VirtAddr {
    VirtAddr::new(general_protection_fault_entry as *const () as u64)
}

/// Loads `frame`'s saved registers and resumes execution at its `rip` via
/// `iretq`. The single entry point used for both a process's very first
/// run and every later resume after preemption.
///
/// # Safety
/// `frame` must describe a state that is actually safe to resume: `rip`
/// must be mapped and executable, `rsp` mapped and writable, in
/// whichever address space is active at the moment this runs, and
/// `cs`/`ss` must be valid ring-3 selectors (this routine always targets
/// ring 3 — the kernel itself is never "resumed" this way, matching the
/// timer entry stub's ring-0 path never routing through here).
pub unsafe fn resume(frame: &TrapFrame) -> ! {
    unsafe {
        core::arch::asm!(
            // Interrupts must stay off from the moment `rsp` starts
            // pointing into `frame` (not a real kernel stack with room
            // for a hardware trap frame) until `iretq` lands us on the
            // process's own stack and restores its `rflags` (which sets
            // `IF` again on its own) — an interrupt firing in between
            // would push its frame into whatever memory happens to
            // follow this `TrapFrame`, corrupting it.
            "cli",
            "mov rsp, {frame}",
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
            frame = in(reg) frame as *const TrapFrame as u64,
            options(noreturn)
        );
    }
}
