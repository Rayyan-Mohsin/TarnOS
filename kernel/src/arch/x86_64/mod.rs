pub mod context_switch;
pub mod gdt;
pub mod idt;
pub mod interrupts;
pub mod syscall;

/// Brings the CPU from Limine's handoff state to a state the rest of the
/// kernel can rely on: our own GDT/TSS (with a dedicated double-fault
/// stack), our own IDT (CPU exceptions plus the timer and COM1 hardware
/// vectors), the PIT/PIC configured with only the timer unmasked, and
/// SYSCALL/SYSRET programmed (needs the GDT's selectors, so it must come
/// after `gdt::init`). Interrupts are enabled only at the very end, once
/// all of that is in place.
pub fn init() {
    gdt::init();
    idt::init();
    unsafe {
        interrupts::init();
    }
    syscall::init();
    x86_64::instructions::interrupts::enable();
}
