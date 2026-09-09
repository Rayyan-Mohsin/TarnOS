pub mod gdt;
pub mod idt;

/// Brings the CPU from Limine's handoff state to a state the rest of the
/// kernel can rely on: our own GDT/TSS (with a dedicated double-fault
/// stack) and our own IDT. Must run before anything that could fault.
pub fn init() {
    gdt::init();
    idt::init();
}
