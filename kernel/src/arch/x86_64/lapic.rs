//! Local APIC (LAPIC) driver: enable + a spurious vector, end-of-interrupt,
//! a targeted send-IPI primitive, and a calibrated per-core periodic
//! timer for forced preemption.
//!
//! Every core also gets its own periodic timer (see
//! [`calibrate_against_pit`]/[`arm_timer_this_core`]), the only thing
//! that can ever preempt a process that never makes a single syscall —
//! the legacy 8259 PIC/PIT can only ever route an interrupt to one core,
//! so before this, only the BSP had any preemption at all. The legacy
//! PIC/PIT (`arch::x86_64::interrupts`) is untouched and keeps driving
//! the BSP's own scheduler-tick bookkeeping (`TICKS`, the `[timer] N
//! ticks` heartbeat) exactly as before — this timer is a second,
//! independent, parallel interrupt path, calibrated against the PIT
//! once but never touching its counter.
use core::sync::atomic::{AtomicU32, Ordering};

use x86_64::registers::model_specific::ApicBase;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};
use x86_64::structures::paging::{Page, PageTableFlags, Size4KiB};
use x86_64::VirtAddr;

use super::percpu;
use crate::earlyprintln;
use crate::memory::virt;

/// Spurious-interrupt vector — clear of both the legacy PIC's 32-47
/// range and [`TEST_IPI_VECTOR`].
const SPURIOUS_VECTOR: u8 = 0xFF;
/// Fixed vector `xtask test-smp-ipi` uses to prove targeted, per-core IPI
/// delivery: the BSP sends this to one specific target LAPIC ID and
/// checks only that core's [`percpu::PerCpuSlot::ipi_count`] advanced.
pub const TEST_IPI_VECTOR: u8 = 0x41;
/// Vector used to wake an idle core with newly-ready work, and to force a
/// core running a `SYS_KILL` target to evict it — see
/// `task::scheduler::terminate_process`/`on_reschedule_ipi` and
/// `arch::x86_64::context_switch::reschedule_entry`. Unlike
/// [`TEST_IPI_VECTOR`] and [`SPURIOUS_VECTOR`] (both handled by an
/// `extern "x86-interrupt"` function via [`register_handlers`]), this one
/// is registered directly in `idt::init`/`idt::load_ap` with a raw entry
/// address, the same way CPU fault vectors are, since it needs the full
/// dual ring0/ring3-dispatching save shape `exception_entry_no_code!`
/// generates -- it may need to force a genuine context switch, not just
/// bump a counter.
pub const RESCHEDULE_VECTOR: u8 = 0x42;
/// This core's own periodic preemption timer — see
/// [`calibrate_against_pit`]/[`arm_timer_this_core`]. Unlike
/// [`RESCHEDULE_VECTOR`] (an IPI another core sends), this one only ever
/// fires locally, but needs the exact same dual ring0/ring3-dispatching
/// stub shape (`context_switch::lapic_timer_entry`) for the same reason:
/// it may need to redirect control to a different process than whatever
/// this core was running when it fired.
pub const LAPIC_TIMER_VECTOR: u8 = 0x43;

const REG_ID: usize = 0x20;
const REG_EOI: usize = 0xB0;
const REG_SVR: usize = 0xF0;
const REG_ICR_LOW: usize = 0x300;
const REG_ICR_HIGH: usize = 0x310;
const REG_LVT_TIMER: usize = 0x320;
const REG_INITIAL_COUNT: usize = 0x380;
const REG_CURRENT_COUNT: usize = 0x390;
const REG_DIVIDE_CONFIG: usize = 0x3E0;

/// Spurious-Interrupt Vector Register bit 8: "APIC software enable."
const SVR_APIC_SOFTWARE_ENABLE: u32 = 1 << 8;
/// Interrupt Command Register bit 14: "Assert" (vs. "De-assert") — the
/// standard shape for a normal fixed-vector IPI send, not an INIT/SIPI.
const ICR_ASSERT: u32 = 1 << 14;
/// LVT Timer Register bit 16: "Mask" — set while calibrating/idle,
/// cleared by [`arm_timer_this_core`] once a real reload value is ready.
const LVT_TIMER_MASKED: u32 = 1 << 16;
/// LVT Timer Register bit 17: "Timer Mode" (1 = periodic, 0 = one-shot).
const LVT_TIMER_PERIODIC: u32 = 1 << 17;
/// Divide Configuration Register value for "divide by 16" (Intel SDM Vol.
/// 3A §11.5.4's 3-bit encoding, split across bits 0-1 and 3): arbitrary
/// but fixed — the same divisor is used for both calibration and the
/// real per-core arm, so it cancels out of the calibrated reload value
/// entirely.
const DIVIDE_BY_16: u32 = 0b0011;
/// Started once at the beginning of the calibration window and read back
/// afterward via `REG_CURRENT_COUNT` — deliberately the largest possible
/// value so the window (bounded by a real PIT tick, not by this counter
/// running out) can never exhaust it first.
const CALIBRATION_SENTINEL: u32 = u32::MAX;

/// The per-core LAPIC-timer reload value [`calibrate_against_pit`]
/// derives once, on the BSP, and every core (BSP included) reads
/// lock-free via [`arm_timer_this_core`]. Zero until calibration has
/// actually run — every real boot path calibrates before any core is
/// ever started (see `arch::x86_64::init`/`smp::bring_up_aps`'s
/// ordering), so an `arm_timer_this_core` call always sees the real
/// value in practice.
static CALIBRATED_INITIAL_COUNT: AtomicU32 = AtomicU32::new(0);

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

/// One-time LAPIC-timer calibration against the legacy PIT, run by the
/// BSP only, right after its own [`init_this_core`] — while the PIT
/// (already programmed by `interrupts::init`) is the only timer running
/// anywhere, and before any AP exists to also be racing this core's own
/// reads of it. Stores the result in [`CALIBRATED_INITIAL_COUNT`] for
/// every later [`arm_timer_this_core`] call (BSP and every AP) to read.
///
/// Synchronizes to the start of a fresh PIT tick before starting the
/// measurement window, rather than starting immediately: this function
/// could be called at any point within whatever tick happens to be
/// running when it's called, and measuring from a partial tick would
/// under-count. Starts a one-shot countdown from [`CALIBRATION_SENTINEL`]
/// (deliberately the largest possible value, so the *PIT's* tick is what
/// bounds the window, never this counter running out first), waits for
/// exactly one full PIT tick, then reads back how much of the sentinel
/// was consumed — that count *is* the reload value for a periodic timer
/// at the PIT's own tick rate, no further scaling needed.
///
/// # Safety
/// Must run after this core's own `init_this_core` and `interrupts::init`
/// (needs the PIT already programmed and only its own IRQ0 line
/// unmasked at the PIC — see that function), before any AP is started,
/// and before this core's own caller (`arch::x86_64::init`) has done
/// anything that would break if a ring0 timer tick fired in the middle
/// of this function (true at the point it's actually called from —
/// nothing there depends on this core's own percpu slot, which is only
/// assigned afterward). This function enables interrupts for its own
/// measurement window (see below) and disables them again before
/// returning, so it does *not* require interrupts to already be
/// disabled, but does leave them disabled again on return either way.
pub unsafe fn calibrate_against_pit() {
    unsafe {
        write_reg(REG_DIVIDE_CONFIG, DIVIDE_BY_16);
        write_reg(REG_LVT_TIMER, LVT_TIMER_MASKED);
    }

    // `interrupts::ticks()` only ever advances from the PIT's own
    // interrupt handler — it cannot tick with interrupts disabled, which
    // is otherwise the state for this entire function's caller
    // (`arch::x86_64::init` only calls its own `sti` at the very end).
    // Enable interrupts for exactly this measurement window — only IRQ0
    // is unmasked at the PIC this early (see `interrupts::init`), so the
    // one interrupt that can land here is a harmless ring0 (kernel, not
    // process) timer tick — then restore the disabled state this
    // function's caller still expects on return.
    x86_64::instructions::interrupts::enable();

    let start = super::interrupts::ticks();
    while super::interrupts::ticks() == start {
        core::hint::spin_loop();
    }

    unsafe { write_reg(REG_INITIAL_COUNT, CALIBRATION_SENTINEL) };
    let window_start = super::interrupts::ticks();
    while super::interrupts::ticks() == window_start {
        core::hint::spin_loop();
    }
    let remaining = unsafe { read_reg(REG_CURRENT_COUNT) };
    unsafe { write_reg(REG_INITIAL_COUNT, 0) };

    x86_64::instructions::interrupts::disable();

    let elapsed = CALIBRATION_SENTINEL - remaining;
    CALIBRATED_INITIAL_COUNT.store(elapsed, Ordering::Release);
    earlyprintln!("[lapic] timer calibrated: {} counts per PIT tick", elapsed);
}

/// Arms this core's own periodic LAPIC timer at [`LAPIC_TIMER_VECTOR`],
/// using the reload value [`calibrate_against_pit`] computed once on the
/// BSP. Must be called by *every* core, including the BSP itself — the
/// LAPIC timer, like every other LAPIC register, is genuinely per-core
/// hardware with no "arm once for every core" shortcut.
///
/// # Safety
/// Must run after this core's own `init_this_core` (needs the LAPIC
/// already enabled) and IDT already loaded (needs `LAPIC_TIMER_VECTOR`
/// registered — see `idt::init`/`idt::load_ap`), and before this core
/// enables interrupts. Must also run after [`calibrate_against_pit`] has
/// completed on the BSP — true of every real boot path (the BSP
/// calibrates before any AP is started; see `arch::x86_64::init`'s and
/// `smp::bring_up_aps`'s ordering).
pub unsafe fn arm_timer_this_core() {
    let reload = CALIBRATED_INITIAL_COUNT.load(Ordering::Acquire);
    unsafe {
        write_reg(REG_DIVIDE_CONFIG, DIVIDE_BY_16);
        write_reg(REG_LVT_TIMER, LVT_TIMER_PERIODIC | LAPIC_TIMER_VECTOR as u32);
        write_reg(REG_INITIAL_COUNT, reload);
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
