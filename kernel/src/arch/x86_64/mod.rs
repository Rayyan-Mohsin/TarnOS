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
/// LAPIC's spurious/test-IPI/reschedule vectors), the PIT/PIC configured
/// with only the timer unmasked, and this core's own LAPIC enabled.
/// Interrupts are enabled only at the very end, once all of that is in
/// place.
///
/// Deliberately does **not** call `syscall::init()` — unlike everything
/// above, it needs `percpu::core_index()` (to program `LSTAR` with this
/// core's own entry-stub copy), which needs this core's own percpu slot
/// already assigned via `percpu::assign_slot`. For the BSP that happens
/// in `smp::bring_up_aps`/`bring_up_bsp_only`, which `main.rs` calls
/// *after* this function — so `main.rs` calls `syscall::init()` itself,
/// right after that. An AP's own slot is always assigned before it's
/// even started, so `smp::ap_entry_on_own_stack` can safely call
/// `syscall::init()` directly as part of its own bring-up.
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
    x86_64::instructions::interrupts::enable();
}
