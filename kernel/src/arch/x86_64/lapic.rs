//! Minimal Local APIC (LAPIC) driver: just enough to bring up additional
//! cores and prove they're independently addressable — enable + a
//! spurious vector, end-of-interrupt, and a targeted send-IPI primitive.
//!
//! No periodic timer. Each core parks in a low-power, interrupt-driven
//! idle loop once it's brought up, waking only when explicitly signaled
//! (an IPI) — a calibrated per-core timer isn't needed until a future
//! milestone actually schedules work across cores, and adds real
//! calibration risk this milestone's scope doesn't need to take on. The
//! legacy PIC/PIT (`arch::x86_64::interrupts`) is untouched and keeps
//! driving the BSP's real scheduler timer exactly as before — this
//! module is a second, independent, parallel interrupt path used only
//! for AP bring-up and the IPI proof.
use core::sync::atomic::Ordering;

use x86_64::registers::model_specific::ApicBase;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};
use x86_64::structures::paging::{Page, PageTableFlags, Size4KiB};
use x86_64::VirtAddr;

use super::percpu;
use crate::memory::virt;

/// Spurious-interrupt vector — clear of both the legacy PIC's 32-47
/// range and [`TEST_IPI_VECTOR`].
const SPURIOUS_VECTOR: u8 = 0xFF;
/// Fixed vector `xtask test-smp-ipi` uses to prove targeted, per-core IPI
/// delivery: the BSP sends this to one specific target LAPIC ID and
/// checks only that core's [`percpu::PerCpuSlot::ipi_count`] advanced.
pub const TEST_IPI_VECTOR: u8 = 0x41;

const REG_ID: usize = 0x20;
const REG_EOI: usize = 0xB0;
const REG_SVR: usize = 0xF0;
const REG_ICR_LOW: usize = 0x300;
const REG_ICR_HIGH: usize = 0x310;

/// Spurious-Interrupt Vector Register bit 8: "APIC software enable."
const SVR_APIC_SOFTWARE_ENABLE: u32 = 1 << 8;
/// Interrupt Command Register bit 14: "Assert" (vs. "De-assert") — the
/// standard shape for a normal fixed-vector IPI send, not an INIT/SIPI.
const ICR_ASSERT: u32 = 1 << 14;

/// Fixed virtual address the LAPIC's MMIO page is explicitly mapped at
/// by [`init_mmio_mapping`] — deliberately *not* reached via
/// `memory::virt::phys_to_virt`/HHDM. Confirmed directly rather than
/// assumed (this exact milestone's own `xtask test-smp-boot` caught the
/// mistake the hard way, as a page fault on the very first LAPIC
/// register write): Limine's HHDM only promises to cover installed RAM,
/// not arbitrary device MMIO holes — the LAPIC's physical base (commonly
/// `0xFEE00000`, but read dynamically below, never hardcoded) is exactly
/// such a hole, not something HHDM is guaranteed to map at all.
const LAPIC_MMIO_VBASE: u64 = 0xffff_9500_0000_0000;

/// Maps the LAPIC's actual physical page — wherever `IA32_APIC_BASE`
/// says it is — at the fixed [`LAPIC_MMIO_VBASE`]. Must run exactly
/// once, on the BSP, before any core (BSP included) calls
/// [`init_this_core`]: every core's LAPIC lives at the same physical
/// address, so one mapping into the shared kernel half of the page
/// tables serves every core identically, the same reasoning
/// `gdt`/`idt`/`smp`'s other per-core-but-eagerly-shared-setup already
/// relies on.
pub fn init_mmio_mapping() {
    let (frame, _flags) = ApicBase::read();
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(LAPIC_MMIO_VBASE));
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_CACHE
        | PageTableFlags::NO_EXECUTE;
    virt::map(page, frame, flags).expect("failed to map the LAPIC's MMIO page");
}

fn mmio_base() -> *mut u32 {
    VirtAddr::new(LAPIC_MMIO_VBASE).as_mut_ptr()
}

/// # Safety
/// `offset` must be a valid LAPIC register offset (one of the `REG_*`
/// constants above), and the LAPIC must already be mapped into this
/// address space's HHDM region — true for every core, since the HHDM
/// covers all usable/reserved physical memory and every core shares the
/// same kernel half of its page tables.
unsafe fn read_reg(offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile(mmio_base().byte_add(offset)) }
}

/// # Safety
/// Same requirement as [`read_reg`].
unsafe fn write_reg(offset: usize, value: u32) {
    unsafe { core::ptr::write_volatile(mmio_base().byte_add(offset), value) };
}

/// Enables this core's LAPIC and programs the spurious-interrupt vector.
/// Must be called once by every core (BSP and every AP) during its own
/// bring-up — the LAPIC is genuinely per-core hardware; there is no
/// "global" enable that covers every core at once.
///
/// # Safety
/// Must run after this core's IDT is loaded (needs [`SPURIOUS_VECTOR`]
/// and [`TEST_IPI_VECTOR`] already registered — see [`register_handlers`]
/// and `idt::init`/`idt::load_ap`), and before this core enables
/// interrupts.
pub unsafe fn init_this_core() {
    unsafe {
        write_reg(REG_SVR, SVR_APIC_SOFTWARE_ENABLE | SPURIOUS_VECTOR as u32);
    }
}

/// Signals end-of-interrupt for whichever LAPIC-sourced interrupt this
/// core is currently handling. Independent of, and does not change, the
/// BSP's existing legacy-PIC-based `interrupts::send_timer_eoi()` path.
pub fn eoi() {
    unsafe { write_reg(REG_EOI, 0) };
}

/// This core's own LAPIC ID, read directly from the LAPIC's ID register.
/// Kept independent of `percpu::core_index()`'s `CPUID`-based read (used
/// only for `smp`'s own landing-state diagnostic, to cross-check the two
/// sources rather than assume they agree).
pub fn this_lapic_id() -> u32 {
    unsafe { read_reg(REG_ID) >> 24 }
}

/// Sends a fixed-vector, physically-addressed IPI to exactly the core
/// whose LAPIC ID is `target_lapic_id`. Deliberately never uses a
/// destination shorthand (self/all/all-but-self) — every send this
/// milestone makes is meant to prove genuinely targeted delivery, not a
/// broadcast.
pub fn send_ipi(target_lapic_id: u32, vector: u8) {
    unsafe {
        write_reg(REG_ICR_HIGH, target_lapic_id << 24);
        write_reg(REG_ICR_LOW, vector as u32 | ICR_ASSERT);
    }
}

/// Spurious interrupts must never be acknowledged with an EOI — see Intel
/// SDM Vol. 3A §11.9: the APIC does not expect one for this specific
/// vector, and sending one anyway is documented as producing undefined
/// behavior on some implementations.
extern "x86-interrupt" fn spurious_handler(_frame: InterruptStackFrame) {}

extern "x86-interrupt" fn test_ipi_handler(_frame: InterruptStackFrame) {
    percpu::slot(percpu::core_index())
        .ipi_count
        .fetch_add(1, Ordering::Relaxed);
    eoi();
}

pub(super) fn register_handlers(idt: &mut InterruptDescriptorTable) {
    idt[SPURIOUS_VECTOR].set_handler_fn(spurious_handler);
    idt[TEST_IPI_VECTOR].set_handler_fn(test_ipi_handler);
}
