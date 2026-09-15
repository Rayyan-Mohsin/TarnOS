//! Interrupt Descriptor Table and CPU exception handlers.
//!
//! Four vectors — divide error, invalid opcode, general-protection
//! fault, page fault — are process-facing: reaching them at CPL 3 means
//! a process misbehaved, not the kernel, and killing just that process
//! (rather than panicking the whole machine) is the entire point of
//! having isolated processes in the first place. Those four are
//! installed via the hand-rolled entry stubs in `context_switch`
//! (`set_handler_addr`, not `set_handler_fn`) instead of the
//! `extern "x86-interrupt"` ABI used everywhere else in this file — see
//! that module's doc comment for why. Breakpoint (non-fatal, always
//! returns) and double fault (always fatal regardless of CPL — it means
//! trap handling itself is already broken, not something safe to try
//! recovering from by killing a process) keep the plain ABI.
use spin::Once;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use super::context_switch::{FaultFrameWithCode, TrapFrame};
use super::gdt::DOUBLE_FAULT_IST_INDEX;
use super::percpu;
use crate::earlyprintln;

static IDT: Once<InterruptDescriptorTable> = Once::new();

pub fn init() {
    let idt = IDT.call_once(|| {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        unsafe {
            idt.divide_error
                .set_handler_addr(super::context_switch::divide_error_entry_addr());
            idt.invalid_opcode
                .set_handler_addr(super::context_switch::invalid_opcode_entry_addr());
            idt.general_protection_fault
                .set_handler_addr(super::context_switch::general_protection_fault_entry_addr());
            idt.page_fault
                .set_handler_addr(super::context_switch::page_fault_entry_addr());
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(DOUBLE_FAULT_IST_INDEX);
            // Same dual ring0/ring3-dispatching stub shape as the four
            // process-facing fault vectors above (`set_handler_addr`, not
            // `set_handler_fn`) since it may need to redirect control to a
            // different process than the one running when the IPI landed
            // -- see `context_switch::reschedule_entry`'s doc comment.
            idt[super::lapic::RESCHEDULE_VECTOR]
                .set_handler_addr(super::context_switch::reschedule_entry_addr());
            // Same dual ring0/ring3-dispatching stub shape, for the same
            // reason -- this core's own periodic preemption timer may
            // need to redirect control to a different process than
            // whatever was running when it fired.
            idt[super::lapic::LAPIC_TIMER_VECTOR]
                .set_handler_addr(super::context_switch::lapic_timer_entry_addr());
        }
        super::interrupts::register_handlers(&mut idt);
        super::lapic::register_handlers(&mut idt);
        idt
    });
    idt.load();
}

/// Loads the already-built shared IDT (see [`init`]) onto an additional
/// core. No new IDT content is needed for this — the IDT is a
/// read-only-after-`init` data structure every core's `IDTR` can point at
/// identically; only the `lidt` instruction itself is genuinely per-core
/// (unlike the GDT's TSS descriptor, see `gdt::init_ap`).
///
/// # Safety
/// Must only be called after [`init`] has completed on the BSP.
pub unsafe fn load_ap() {
    IDT.get()
        .expect("idt::init() must run before idt::load_ap()")
        .load();
}

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    earlyprintln!("[int3] breakpoint at {:#x}", stack_frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) -> ! {
    panic!(
        "DOUBLE FAULT (error code {:#x}) at {:#x}",
        error_code,
        stack_frame.instruction_pointer.as_u64()
    );
}

/// Common tail for every process-facing exception's ring-3 path: names
/// which process is being killed and why, then hands off to the
/// scheduler exactly as `SYS_EXIT` would (see
/// `task::scheduler::terminate_current_process`) — a fault that kills a
/// process is, from the scheduler's point of view, indistinguishable
/// from that process exiting itself.
fn kill_faulting_process_and_reschedule(description: core::fmt::Arguments) -> *mut TrapFrame {
    match crate::task::scheduler::with_current_process(|p| p.pid) {
        Some(pid) => earlyprintln!("[fault] pid {} killed: {}", pid.index(), description),
        None => earlyprintln!("[fault] killed (no current process?): {}", description),
    }
    crate::task::scheduler::terminate_current_process(tarnos_abi::ExitStatus::Faulted)
}

#[unsafe(no_mangle)]
pub extern "C" fn divide_error_ring0(frame: *mut TrapFrame) -> ! {
    let rip = unsafe { (*frame).rip };
    panic!("divide error at {:#x}", rip);
}

#[unsafe(no_mangle)]
pub extern "C" fn divide_error_ring3(frame: *mut TrapFrame) -> *mut TrapFrame {
    let rip = unsafe { (*frame).rip };
    kill_faulting_process_and_reschedule(format_args!("divide error at {:#x}", rip))
}

#[unsafe(no_mangle)]
pub extern "C" fn invalid_opcode_ring0(frame: *mut TrapFrame) -> ! {
    let rip = unsafe { (*frame).rip };
    panic!("invalid opcode at {:#x}", rip);
}

#[unsafe(no_mangle)]
pub extern "C" fn invalid_opcode_ring3(frame: *mut TrapFrame) -> *mut TrapFrame {
    let rip = unsafe { (*frame).rip };
    kill_faulting_process_and_reschedule(format_args!("invalid opcode at {:#x}", rip))
}

#[unsafe(no_mangle)]
pub extern "C" fn general_protection_fault_ring0(frame: *mut FaultFrameWithCode) -> ! {
    let (rip, error_code) = unsafe { ((*frame).rip, (*frame).error_code) };
    let core = percpu::core_index();
    let current_raw = percpu::slot(core).current.load(core::sync::atomic::Ordering::Acquire);
    crate::task::scheduler::dump_cores_for_panic();
    // See `page_fault_ring0`'s matching comment: naming which process-table
    // slot's kernel stack `rip` itself falls in (when it does) is direct
    // evidence for the still-open cross-core corruption bug (`docs/adr/0013`)
    // without needing a live-GDB session to decode it by hand.
    match crate::task::process::describe_kernel_stack_address(rip) {
        Some((slot, offset)) => panic!(
            "general protection fault (error code {:#x}) at {:#x} -- core {core} was running raw \
             pid {current_raw:#x}; the faulting rip is kernel-stack slot {slot} (offset {offset:#x} \
             from its own top)",
            error_code, rip
        ),
        None => panic!(
            "general protection fault (error code {:#x}) at {:#x} -- core {core} was running raw \
             pid {current_raw:#x}",
            error_code, rip
        ),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn general_protection_fault_ring3(
    frame: *mut FaultFrameWithCode,
) -> *mut TrapFrame {
    let (rip, error_code) = unsafe { ((*frame).rip, (*frame).error_code) };
    kill_faulting_process_and_reschedule(format_args!(
        "general protection fault (error code {:#x}) at {:#x}",
        error_code, rip
    ))
}

#[unsafe(no_mangle)]
pub extern "C" fn page_fault_ring0(frame: *mut FaultFrameWithCode) -> ! {
    let rip = unsafe { (*frame).rip };
    let error_code = PageFaultErrorCode::from_bits_truncate(unsafe { (*frame).error_code });
    let fault_addr = Cr2::read().map(|a| a.as_u64()).unwrap_or(0);
    let core = percpu::core_index();
    // Lock-free (`percpu::PerCpuSlot.current`'s whole reason to exist) --
    // safe to read from a panic handler that must never risk contending
    // (or deadlocking on) `SCHEDULER` itself.
    let current_raw = percpu::slot(core).current.load(core::sync::atomic::Ordering::Acquire);
    crate::task::scheduler::dump_cores_for_panic();
    // Naming which process-table slot's kernel stack `fault_addr` itself
    // falls in (when it does) turns the still-open cross-core corruption
    // bug (`docs/adr/0013`) -- previously decoded by hand from a raw hex
    // address via a live-GDB session -- into something this panic message
    // states directly: which stack got a bad return address landed on it,
    // and which pid this faulting core itself was running when it happened.
    match crate::task::process::describe_kernel_stack_address(fault_addr) {
        Some((slot, offset)) => panic!(
            "page fault accessing {:#x} (error {:?}) at {:#x} -- core {core} was running raw pid \
             {current_raw:#x}; the faulting address is kernel-stack slot {slot} (offset \
             {offset:#x} from its own top)",
            fault_addr, error_code, rip
        ),
        None => panic!(
            "page fault accessing {:#x} (error {:?}) at {:#x} -- core {core} was running raw pid \
             {current_raw:#x}",
            fault_addr, error_code, rip
        ),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn page_fault_ring3(frame: *mut FaultFrameWithCode) -> *mut TrapFrame {
    let rip = unsafe { (*frame).rip };
    let error_code = PageFaultErrorCode::from_bits_truncate(unsafe { (*frame).error_code });
    let fault_addr = Cr2::read().map(|a| a.as_u64()).unwrap_or(0);
    kill_faulting_process_and_reschedule(format_args!(
        "page fault accessing {:#x} (error {:?}) at {:#x}",
        fault_addr, error_code, rip
    ))
}
