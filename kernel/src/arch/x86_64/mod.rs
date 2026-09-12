pub mod context_switch;
pub mod gdt;
pub mod idt;
pub mod interrupts;
pub mod lapic;
pub mod percpu;
pub mod smp;
pub mod syscall;

/// Brings the CPU from Limine's handoff state to a state the rest of the
/// kernel can rely on: our own GDT/TSS (with a dedicated double-fault
/// stack, and TSS descriptor slots for every possible core — see `gdt`),
/// our own IDT (CPU exceptions, the timer/COM1 hardware vectors, and the
/// LAPIC's spurious/test-IPI vectors), the PIT/PIC configured with only
/// the timer unmasked, this core's own LAPIC enabled, and SYSCALL/SYSRET
/// programmed (needs the GDT's selectors, so it must come after
/// `gdt::init_bsp`). Interrupts are enabled only at the very end, once
/// all of that is in place.
///
/// Only brings up the BSP itself — call `smp::bring_up_aps` afterward to
/// start every additional core Limine reported.
pub fn init() {
    gdt::init_bsp();
    idt::init();
    unsafe {
        interrupts::init();
    }
    // Must run before the first `lapic::init_this_core()` call (right
    // below, for the BSP itself) -- every core's LAPIC lives at the same
    // physical address, so this one mapping (into the shared kernel
    // half of the page tables) serves every core, BSP and every AP
    // alike, once and for all.
    lapic::init_mmio_mapping();
    unsafe {
        lapic::init_this_core();
    }
    syscall::init();
    x86_64::instructions::interrupts::enable();
}
