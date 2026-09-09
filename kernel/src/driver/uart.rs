//! 16550 UART driver (COM1, port 0x3F8).
//!
//! Unlike the bare port poke in `earlycon` (kept as the fallback used by
//! panics and boot messages, which must work even before this driver is
//! initialized), this does the real 16550 initialization sequence and
//! checks LSR before every transmit — required on real hardware even
//! though QEMU's emulation tolerates skipping it.
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use x86_64::instructions::port::Port;

use super::{CharDevice, Driver, InterruptHandler};
use crate::arch::x86_64::interrupts;
use crate::sync::SpinLock;

const COM1_BASE: u16 = 0x3F8;

const IER_RX_AVAILABLE: u8 = 0x01;
const FCR_ENABLE_FIFO_CLEAR: u8 = 0xC7;
const LCR_8N1: u8 = 0x03;
const LCR_DLAB: u8 = 0x80;
const MCR_INIT: u8 = 0x0B; // DTR | RTS | OUT2 (OUT2 gates the IRQ line to the PIC)
const LSR_DATA_READY: u8 = 0x01;
const LSR_THR_EMPTY: u8 = 0x20;

/// Divisor for 38400 baud from the UART's 115200 Hz base clock. Plenty
/// for a debug console; QEMU ignores it entirely but real hardware and
/// serial-over-USB adapters do not.
const BAUD_DIVISOR: u16 = 3;

pub struct Uart16550 {
    data: Port<u8>,
    interrupt_enable: Port<u8>,
    fifo_control: Port<u8>,
    line_control: Port<u8>,
    modem_control: Port<u8>,
    line_status: Port<u8>,
}

impl Uart16550 {
    const fn new(base: u16) -> Self {
        Self {
            data: Port::new(base),
            interrupt_enable: Port::new(base + 1),
            fifo_control: Port::new(base + 2),
            line_control: Port::new(base + 3),
            modem_control: Port::new(base + 4),
            line_status: Port::new(base + 5),
        }
    }

    fn init(&mut self) {
        unsafe {
            self.interrupt_enable.write(0x00u8);

            self.line_control.write(LCR_DLAB);
            let mut divisor_lo = Port::<u8>::new(COM1_BASE);
            let mut divisor_hi = Port::<u8>::new(COM1_BASE + 1);
            divisor_lo.write((BAUD_DIVISOR & 0xFF) as u8);
            divisor_hi.write((BAUD_DIVISOR >> 8) as u8);

            self.line_control.write(LCR_8N1);
            self.fifo_control.write(FCR_ENABLE_FIFO_CLEAR);
            self.modem_control.write(MCR_INIT);
            self.interrupt_enable.write(IER_RX_AVAILABLE);
        }
    }

    fn line_status(&mut self) -> u8 {
        unsafe { self.line_status.read() }
    }
}

impl Driver for Uart16550 {
    fn name(&self) -> &'static str {
        "uart16550(com1)"
    }
}

impl CharDevice for Uart16550 {
    fn write_byte(&mut self, byte: u8) {
        while self.line_status() & LSR_THR_EMPTY == 0 {
            core::hint::spin_loop();
        }
        unsafe {
            self.data.write(byte);
        }
    }

    fn try_read_byte(&mut self) -> Option<u8> {
        if self.line_status() & LSR_DATA_READY != 0 {
            Some(unsafe { self.data.read() })
        } else {
            None
        }
    }
}

impl InterruptHandler for Uart16550 {
    fn handle_irq(&mut self) {
        // Only wake whoever is waiting — never touch the FIFO or run task
        // code here. Reading the byte and echoing it back both happen at
        // task-poll time (see `RxAvailable` / `echo_task` below), in the
        // kernel idle loop, not in interrupt context.
        if let Some(waker) = RX_WAKER.lock().take() {
            waker.wake();
        }
    }
}

// `COM1` is locked from interrupt context (`handle_irq`, below) as well
// as normal code (`write_bytes`), so it must use the interrupt-disabling
// `SpinLock` rather than a plain `spin::Mutex` — otherwise normal code
// holding the lock could be interrupted by the very ISR that also wants
// it, deadlocking this core against itself.
static COM1: SpinLock<Uart16550> = SpinLock::new(Uart16550::new(COM1_BASE));
static RX_WAKER: SpinLock<Option<Waker>> = SpinLock::new(None);

fn handle_irq() {
    COM1.lock().handle_irq();
}

/// Initializes COM1 and unmasks IRQ4 at the PIC. Only once the device is
/// actually configured to raise RX-available interrupts (done inside
/// `Uart16550::init`) is it safe to let that line fire.
pub fn init() {
    COM1.lock().init();
    super::register_irq(interrupts::IRQ4, handle_irq);
    interrupts::unmask_irq4();
}

pub fn write_bytes(bytes: &[u8]) {
    COM1.lock().write_bytes(bytes);
}

pub fn write_byte(byte: u8) {
    COM1.lock().write_byte(byte);
}

/// Resolves to the next received byte, exercising the full
/// interrupt -> `Waker` -> executor -> driver path rather than polling.
struct RxAvailable;

impl Future for RxAvailable {
    type Output = u8;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u8> {
        if let Some(byte) = COM1.lock().try_read_byte() {
            return Poll::Ready(byte);
        }
        // Register interest, then re-check: a byte (and the interrupt
        // that would have woken us) may have arrived in the gap between
        // the check above and this registration, and we would otherwise
        // miss that wakeup and hang forever.
        *RX_WAKER.lock() = Some(cx.waker().clone());
        match COM1.lock().try_read_byte() {
            Some(byte) => Poll::Ready(byte),
            None => Poll::Pending,
        }
    }
}

/// A standalone kernel task that echoes every received byte back out,
/// entirely via the async interrupt -> waker -> executor -> driver path
/// (no polling loop, no work done inside interrupt context) — proof that
/// the executor's wake bridge actually works, not just that the UART can
/// transmit and receive.
pub async fn echo_task() {
    loop {
        let byte = RxAvailable.await;
        write_byte(byte);
    }
}
