//! Driver framework.
//!
//! Small, composable traits expressed only in plain data (`&[u8]`, `u8`) —
//! never kernel-internal types (page tables, process handles, capabilities).
//! That is what lets a driver eventually move out of the kernel into its
//! own isolated process: a consumer written against [`CharDevice`] cannot
//! tell whether calls are serviced in-kernel or forwarded over IPC to a
//! driver process, because the trait was never given a way to ask.
//!
//! UART stays kernel-resident this milestone (boot diagnostics need it
//! before process infrastructure exists), registered in the small IRQ
//! dispatch table below rather than wired ad hoc — that table is the seed
//! of a future out-of-process driver manager, not a one-off.
pub mod uart;

use spin::Mutex;

/// Lifecycle every driver implements.
pub trait Driver {
    fn name(&self) -> &'static str;
}

/// A byte-oriented device: UART today, a future PTY or virtio-console
/// later. Deliberately has no notion of "is this local or remote" —
/// see the module doc comment.
pub trait CharDevice: Driver {
    fn write_byte(&mut self, byte: u8);

    fn write_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_byte(b);
        }
    }

    fn try_read_byte(&mut self) -> Option<u8>;
}

/// Implemented by whatever handles a device's hardware interrupt.
pub trait InterruptHandler {
    fn handle_irq(&mut self);
}

const MAX_IRQ: usize = 16;

/// One dispatch slot per legacy IRQ line. Holds a plain function pointer
/// rather than `dyn InterruptHandler` because handlers here own no state
/// of their own — they close over a specific global driver instance (see
/// `uart::handle_irq`) and call its `InterruptHandler` implementation
/// after locking it, which a trait object tied to `&'static` storage
/// cannot express as simply for a singleton device.
static IRQ_TABLE: Mutex<[Option<fn()>; MAX_IRQ]> = Mutex::new([None; MAX_IRQ]);

/// Registers `handler` to run when IRQ `irq` is dispatched. Does not touch
/// the PIC mask — a driver unmasks its own line only once it is actually
/// ready to handle the interrupt (see `arch::x86_64::interrupts`).
pub fn register_irq(irq: u8, handler: fn()) {
    IRQ_TABLE.lock()[irq as usize] = Some(handler);
}

/// Runs the handler registered for `irq`, if any. Called from the
/// architecture's low-level interrupt handlers; PIC end-of-interrupt is
/// the caller's responsibility, not the driver's.
pub fn dispatch_irq(irq: u8) {
    if let Some(handler) = IRQ_TABLE.lock()[irq as usize] {
        handler();
    }
}
