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
    let core = percpu::core_index();
    let current_raw = percpu::slot(core).current.load(core::sync::atomic::Ordering::Acquire);
    crate::task::scheduler::dump_cores_for_panic();
    let rip = stack_frame.instruction_pointer.as_u64();
    match crate::task::process::describe_kernel_stack_address(rip) {
        Some((slot, offset)) => panic!(
            "DOUBLE FAULT (error code {:#x}) at {:#x} -- core {core} was running raw pid \
             {current_raw:#x}; the faulting rip is kernel-stack slot {slot} (offset {offset:#x} \
             from its own top)",
            error_code, rip
        ),
        None => panic!(
            "DOUBLE FAULT (error code {:#x}) at {:#x} -- core {core} was running raw pid \
             {current_raw:#x}",
            error_code, rip
        ),
    }
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

/// Renders `describe_kernel_stack_address`'s result for a panic message,
/// or `"outside any kernel stack"` if `addr` doesn't land in one — shared
/// by the GP-fault and page-fault ring0 handlers so both can report not
/// just the faulting `rip`'s own attribution but `rsp`'s too: if a core
/// is genuinely executing *on* a kernel stack that isn't the one its own
/// `current` pid names, `rsp` itself (not just some corrupted value that
/// got fetched as if it were code) will show that directly, which no
/// previous diagnostic in this investigation (`docs/adr/0013`-`0017`)
/// captured — every prior capture only ever showed a bad *value* landing
/// somewhere, never whether the core's own live stack pointer was
/// already on the wrong stack before that value was even fetched.
fn describe_stack_addr(addr: u64) -> alloc::string::String {
    match crate::task::process::describe_kernel_stack_address(addr) {
        // `offset` is measured from the slot's own *base* (just above its
        // guard page), not its top -- see that function's own doc
        // comment on `KERNEL_STACK_SLOT_STRIDE`.
        Some((slot, offset)) => alloc::format!("kernel-stack slot {slot} (offset {offset:#x} from its own base)"),
        None => alloc::string::String::from("outside any kernel stack"),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn general_protection_fault_ring0(frame: *mut FaultFrameWithCode) -> ! {
    let (rip, error_code) = unsafe { ((*frame).rip, (*frame).error_code) };
    // This handler only ever runs when the interrupted context was
    // already in ring 0 (`exception_entry_with_code!`'s `test al, 3`
    // branch) -- a same-privilege exception, for which the CPU never
    // pushes RSP/SS at all (SDM Vol. 3 6.13: those two words only exist
    // when the exception also raises the privilege level). Reading
    // `(*frame).rsp` here would dereference memory the CPU never wrote --
    // whatever stale bytes already happened to sit on this stack below
    // the frame it actually pushed, not a real value; every prior
    // capture using that field (docs/adr/0018, 0019) was comparing a
    // genuine `rip` against noise, not real evidence of which stack this
    // core was on. The real rsp this core had at the moment of the fault
    // is recovered as a pure address computation instead: `frame`'s own
    // address, plus the byte offset the (unwritten) `rsp` field would
    // occupy, is exactly where the CPU's rsp was pointing right before
    // it took this exception -- a same-privilege exception never moves
    // the stack, so that address is real and correct even though nothing
    // was ever stored there.
    let rsp = frame as u64 + core::mem::offset_of!(FaultFrameWithCode, rsp) as u64;
    let core = percpu::core_index();
    let current_raw = percpu::slot(core).current.load(core::sync::atomic::Ordering::Acquire);
    crate::task::scheduler::dump_cores_for_panic();
    // See `page_fault_ring0`'s matching comment: naming which process-table
    // slot's kernel stack `rip` itself falls in (when it does) is direct
    // evidence for the still-open cross-core corruption bug (`docs/adr/0013`)
    // without needing a live-GDB session to decode it by hand.
    panic!(
        "general protection fault (error code {:#x}) at {:#x} -- core {core} was running raw pid \
         {current_raw:#x}; the faulting rip is {}; rsp ({:#x}) is {}",
        error_code,
        rip,
        describe_stack_addr(rip),
        rsp,
        describe_stack_addr(rsp)
    );
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
    // See `general_protection_fault_ring0`'s matching comment: this
    // handler only runs for a same-privilege (ring0 -> ring0) exception,
    // for which the CPU never pushes RSP/SS, so `(*frame).rsp` would be
    // stale stack noise rather than a real value. `rsp` is instead
    // recovered as the address the CPU's real rsp had at fault time (a
    // same-privilege exception never moves the stack).
    let rsp = frame as u64 + core::mem::offset_of!(FaultFrameWithCode, rsp) as u64;
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
    // Also reports `rsp`'s own attribution (see `describe_stack_addr`'s
    // doc comment, and this function's own comment on how `rsp` is now
    // computed) -- now a real address rather than the stale-stack-memory
    // garbage this diagnostic read before the fix above (docs/adr/0020):
    // every earlier capture's "current names one pid, rsp sits inside a
    // different one's stack" observation (docs/adr/0018, 0019) needs to
    // be treated as unreliable, since it was comparing a genuine `rip`
    // against noise. The next capture is the first trustworthy one.
    panic!(
        "page fault accessing {:#x} (error {:?}) at {:#x} -- core {core} was running raw pid \
         {current_raw:#x}; the faulting address is {}; rsp ({:#x}) is {}",
        fault_addr,
        error_code,
        rip,
        describe_stack_addr(fault_addr),
        rsp,
        describe_stack_addr(rsp)
    );
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
