//! Hardware interrupt plumbing: the legacy 8259 PIC (remapped clear of the
//! CPU exception vectors) and the PIT timer tick.
//!
//! ACPI/MADT parsing and the LAPIC/IOAPIC migration this implies are
//! deferred to the SMP milestone — the RSDP is already captured at boot
//! (see `main.rs`) specifically so that migration is additive later
//! rather than requiring a boot-protocol change now.
use core::sync::atomic::{AtomicU64, Ordering};
use pic8259::ChainedPics;
use x86_64::instructions::port::Port;
use x86_64::structures::idt::InterruptStackFrame;

use crate::earlyprintln;
use crate::sync::SpinLock;

/// The primary PIC is remapped so IRQ0-7 land on vectors 32-39, clear of
/// the CPU's 32 reserved exception vectors (0-31); the secondary PIC
/// follows immediately after at 40-47.
const PIC_1_OFFSET: u8 = 32;
const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

pub const TIMER_IRQ: u8 = 0;
pub const IRQ4: u8 = 4; // COM1

pub const TIMER_VECTOR: u8 = PIC_1_OFFSET + TIMER_IRQ;
pub const IRQ4_VECTOR: u8 = PIC_1_OFFSET + IRQ4;

// `SpinLock`, not a plain `spin::Mutex`: `send_timer_eoi` and
// `irq4_handler` both lock this from interrupt context, and `unmask_irq4`
// below locks it twice in a row from normal code. A plain mutex left a
// real, if narrow, self-deadlock window — a timer interrupt landing on
// this core in between those two normal-code locks would spin forever in
// its own EOI trying to re-acquire a lock only the now-interrupted code
// could release. Observed in practice as an intermittent boot hang right
// at UART init (the only caller of `unmask_irq4`), fixed by this type.
static PICS: SpinLock<ChainedPics> =
    SpinLock::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Number of timer ticks since [`init`]. Will back scheduler timeslice
/// accounting once a scheduler exists; for now it only drives the
/// heartbeat print that proves interrupts are actually firing.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

const PIT_FREQUENCY_HZ: u32 = 100;
const PIT_BASE_FREQUENCY_HZ: u32 = 1_193_182;

/// Programs PIT channel 0 for a periodic tick at [`PIT_FREQUENCY_HZ`] and
/// remaps + configures the PICs so only IRQ0 (timer) starts unmasked.
/// IRQ4 (COM1) is registered in the IDT (see `idt::init`) but stays
/// masked here — the UART driver (a later milestone task) unmasks it
/// once it has actually enabled RX-available interrupts on the device,
/// so nothing can fire on that line before anything is listening.
///
/// # Safety
/// Must run after the IDT (with handlers for [`TIMER_VECTOR`] and
/// [`IRQ4_VECTOR`] installed) is loaded, and before interrupts are
/// enabled with `sti`.
pub unsafe fn init() {
    unsafe {
        PICS.lock().initialize();
        // Unmask IRQ0 only (bit 0 clear); every other primary-PIC line
        // and the entire secondary PIC stay masked until something is
        // actually ready to handle them.
        PICS.lock().write_masks(0b1111_1110, 0b1111_1111);
    }

    let divisor = (PIT_BASE_FREQUENCY_HZ / PIT_FREQUENCY_HZ) as u16;
    unsafe {
        let mut command: Port<u8> = Port::new(0x43);
        let mut channel0: Port<u8> = Port::new(0x40);
        // Channel 0, lobyte/hibyte access mode, mode 2 (rate generator).
        command.write(0b0011_0100u8);
        channel0.write((divisor & 0xFF) as u8);
        channel0.write((divisor >> 8) as u8);
    }
}

/// Bumps the tick counter and prints the heartbeat. Called from
/// `context_switch`'s hand-written timer entry stub — the timer vector
/// cannot use a plain `extern "x86-interrupt" fn` like the other
/// handlers here, because that ABI hides the general-purpose registers
/// from us, and a scheduler resuming a *different* process than the one
/// interrupted needs full control over exactly which registers get
/// restored on the way out.
pub(super) fn on_timer_tick_bookkeeping() {
    let n = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    // One line roughly once a second, just enough to prove ticks keep
    // arriving without flooding the serial console at 100 Hz.
    if n % (PIT_FREQUENCY_HZ as u64) == 0 {
        earlyprintln!("[timer] {} ticks", n);
    }
}

pub(super) fn send_timer_eoi() {
    unsafe {
        PICS.lock().notify_end_of_interrupt(TIMER_VECTOR);
    }
}

/// Dispatches to whatever the driver framework registered for IRQ4 (the
/// UART driver, once it has initialized the device and unmasked this
/// line via [`unmask_irq4`]). Registered in the IDT unconditionally, but
/// masked at the PIC until then, so nothing can call in with no handler
/// registered.
extern "x86-interrupt" fn irq4_handler(_stack_frame: InterruptStackFrame) {
    crate::driver::dispatch_irq(IRQ4);
    unsafe {
        PICS.lock().notify_end_of_interrupt(IRQ4_VECTOR);
    }
}

/// Unmasks IRQ4 at the PIC. Called by the UART driver only after it has
/// finished initializing the device and enabling its RX-available
/// interrupt — never before, since unmasking first would let the line
/// fire before `driver::register_irq` has anything registered for it.
pub fn unmask_irq4() {
    unsafe {
        let [mask1, mask2] = PICS.lock().read_masks();
        PICS.lock().write_masks(mask1 & !(1 << IRQ4), mask2);
    }
}

pub(super) fn register_handlers(idt: &mut x86_64::structures::idt::InterruptDescriptorTable) {
    // SAFETY: `timer_interrupt_entry` is a valid interrupt-vector entry
    // point (see `context_switch`): it saves every general-purpose
    // register on entry and restores them before `iretq`, matching what
    // an interrupt gate requires.
    unsafe {
        idt[TIMER_VECTOR].set_handler_addr(super::context_switch::timer_interrupt_entry_addr());
    }
    idt[IRQ4_VECTOR].set_handler_fn(irq4_handler);
}
